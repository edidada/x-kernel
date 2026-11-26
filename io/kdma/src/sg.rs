// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scatter-Gather (SG) and streaming DMA mapping.
//!
//! `dma_map_sg` takes a list of physically discontiguous page ranges and
//! produces bus addresses the device can program into its descriptors:
//!
//! * **Direct mapping** — when the segment fits the device's `dma_mask`, does
//!   not cross `segment_boundary_mask`, and is cache-safe (coherent platform,
//!   or cache-line aligned so maintenance cannot touch neighbouring bytes),
//!   the bus address is the physical address translated by the platform's
//!   bus offset. Direct segments are merged while physically contiguous and
//!   the hardware limits allow it, so `nents` may shrink below
//!   `orig_nents` — descriptors are programmed from `bus_addr`/`len` pairs of
//!   the first `nents` segments.
//! * **Per-page bounce fallback** — segments that fail any direct-mapping
//!   test are staged through the coherent bounce pool one page run at a
//!   time; normal pages keep direct mapping, so a single high page does not
//!   bounce the whole table.
//!
//! Cache maintenance runs through [`kiface_mm_api::DmaCacheIf`]: clean
//! before the device reads (`DMA_TO_DEVICE`, `sync_for_device`), clean +
//! invalidate before the CPU reads device results (`DMA_FROM_DEVICE`,
//! `sync_for_cpu`). Bounced segments need no maintenance — the coherent pool
//! already guarantees visibility.

use alloc::vec::Vec;
use core::ptr::NonNull;

use alloc_engine::AllocResult;
use khal::mem::p2v;
use memaddr::PhysAddr;

use crate::{DmaBusAddress, DmaDirection, dma::ALLOCATOR};

/// Device DMA hardware limits for one mapping request.
#[derive(Clone, Copy, Debug)]
pub struct DmaConstraints {
    /// Highest device-reachable bus address (`DMA_BIT_MASK(n)`).
    pub dma_mask: u64,
    /// Longest single segment the device descriptors can express, in bytes.
    pub max_segment_size: usize,
    /// Segments must not cross `segment_boundary_mask` boundaries: for the
    /// whole segment, `(addr | mask) == ((addr + len - 1) | mask)`.
    pub segment_boundary_mask: u64,
    /// Cache-line size for non-coherent safety checks; coherent devices
    /// ignore it.
    pub cache_line_size: usize,
    /// Whether the device snoops CPU caches (no maintenance required).
    pub cache_coherent: bool,
}

impl DmaConstraints {
    /// Constraints of a fully capable 64-bit coherent device.
    pub const UNCONSTRAINED: Self = Self {
        dma_mask: u64::MAX,
        max_segment_size: usize::MAX,
        segment_boundary_mask: u64::MAX,
        cache_line_size: 64,
        cache_coherent: true,
    };

    fn crosses_boundary(&self, bus: u64, len: usize) -> bool {
        if self.segment_boundary_mask == u64::MAX {
            return false;
        }
        let boundary = self.segment_boundary_mask | 1;
        let stride = boundary + 1;
        let first = bus / stride;
        let last = (bus + len as u64 - 1) / stride;
        first != last
    }

    fn fits_mask(&self, bus: u64, len: usize) -> bool {
        bus.checked_add(len as u64 - 1)
            .is_some_and(|last| last <= self.dma_mask)
    }

    fn cache_safe(&self, bus: u64, len: usize) -> bool {
        if self.cache_coherent {
            return true;
        }
        let line = self.cache_line_size.max(1) as u64;
        bus.is_multiple_of(line) && (len as u64).is_multiple_of(line)
    }
}

/// One input extent: a physically contiguous byte range.
#[derive(Clone, Copy, Debug)]
pub struct SgEntry {
    /// First physical address of the extent.
    pub paddr: PhysAddr,
    /// Extent length in bytes.
    pub len: usize,
}

impl SgEntry {
    /// Describes one extent.
    pub const fn new(paddr: PhysAddr, len: usize) -> Self {
        Self { paddr, len }
    }
}

/// One segment of an SG table: input extent plus mapped bus address.
///
/// Before mapping, `bus_addr` is meaningless; after [`dma_map_sg`], the
/// first `nents` segments carry valid bus addresses and merged segment
/// lengths, while the input extent (`page_paddr`/`input_len`) is preserved
/// for unmap and diagnostics.
#[derive(Clone, Copy, Debug)]
pub struct SgSegment {
    /// Input: first physical address of the original extent.
    pub page_paddr: PhysAddr,
    /// Mapped: bus address the device uses (direct translation or bounce
    /// pool). Valid only for mapped segments.
    pub bus_addr: DmaBusAddress,
    /// Mapped: length in bytes of this segment as the device sees it.
    /// `dma_map_sg` overwrites the input length here while merging.
    pub len: usize,
    /// Input: original extent length, kept for unmap and diagnostics.
    pub input_len: usize,
}

impl SgSegment {
    /// The mapped segment's exclusive end bus address.
    pub const fn bus_end(&self) -> u64 {
        self.bus_addr.as_u64() + self.len as u64
    }
}

/// How one mapped segment's data path was satisfied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MapPath {
    /// Bus address is the physical translation; cache maintenance applies on
    /// non-coherent platforms.
    Direct,
    /// Data was staged through the coherent bounce pool; `bus_addr` names the
    /// pool buffer.
    Bounced,
}

/// SG mapping bookkeeping for one table, private to map/unmap.
#[derive(Clone, Copy, Debug)]
struct MapState {
    path: MapPath,
}

/// Scatter-gather descriptor table.
///
/// Built from [`SgEntry`] extents, mapped with [`dma_map_sg`], consumed by
/// the driver (first `nents` segments), and released with [`dma_unmap_sg`].
#[derive(Clone, Debug)]
pub struct SgTable {
    segs: Vec<SgSegment>,
    /// Mapped (post-merge) segment count; `0` while unmapped.
    nents: usize,
    /// Input extent count, never changed by mapping.
    orig_nents: usize,
    state: Vec<MapState>,
}

impl SgTable {
    /// Builds a table from input extents.
    ///
    /// # Errors
    /// Returns `AllocError::InvalidInput` for a zero-length extent.
    pub fn from_entries(entries: &[SgEntry]) -> AllocResult<Self> {
        if entries.is_empty() || entries.iter().any(|entry| entry.len == 0) {
            return Err(alloc_engine::AllocError::InvalidInput);
        }
        let segs = entries
            .iter()
            .map(|entry| SgSegment {
                page_paddr: entry.paddr,
                bus_addr: DmaBusAddress::default(),
                len: entry.len,
                input_len: entry.len,
            })
            .collect();
        let orig_nents = entries.len();
        Ok(Self {
            segs,
            nents: 0,
            orig_nents,
            state: Vec::new(),
        })
    }

    /// Builds a table from one page list where every page is `PAGE_SIZE`.
    pub fn from_pages(page_paddings: &[PhysAddr], page_size: usize) -> AllocResult<Self> {
        let entries: Vec<SgEntry> = page_paddings
            .iter()
            .map(|paddr| SgEntry::new(*paddr, page_size))
            .collect();
        Self::from_entries(&entries)
    }

    /// Mapped segment count (may be less than [`Self::orig_nents`] after
    /// merging).
    pub const fn nents(&self) -> usize {
        self.nents
    }

    /// Input extent count.
    pub const fn orig_nents(&self) -> usize {
        self.orig_nents
    }

    /// Mapped segment slice: valid after [`dma_map_sg`], first `nents`
    /// entries are the device-programmable segments.
    pub fn segments(&self) -> &[SgSegment] {
        &self.segs
    }
}

/// Whether one extent can map directly under `constraints`.
fn can_map_direct(entry: &SgSegment, constraints: &DmaConstraints) -> bool {
    let bus = crate::p2b(entry.page_paddr).as_u64();
    constraints.fits_mask(bus, entry.input_len)
        && !constraints.crosses_boundary(bus, entry.input_len)
        && constraints.cache_safe(bus, entry.input_len)
}

/// Maps one extent directly.
fn map_direct(segment: &mut SgSegment) {
    segment.bus_addr = crate::p2b(segment.page_paddr);
    segment.len = segment.input_len;
}

/// Cache maintenance for one direct segment.
///
/// # Safety
///
/// The segment's physical range must be kernel RAM on the linear map and
/// owned by the mapping.
unsafe fn sync_direct(segment: &SgSegment, invalidate: bool) {
    let vaddr = p2v(segment.page_paddr).as_usize();
    if invalidate {
        kplat::dma::clean_inval_range(vaddr, segment.input_len);
    } else {
        kplat::dma::clean_range(vaddr, segment.input_len);
    }
}

/// Maps a scatter-gather table for one device and direction.
///
/// Returns the post-merge segment count (`nents`); descriptors must be
/// programmed from the first `nents` [`SgSegment`]s. Segments that cannot map
/// directly (addressing limits, boundary crossing, cache-unsafe on a
/// non-coherent platform) fall back to the coherent bounce pool per extent.
///
/// # Safety
///
/// The table's physical extents must be live kernel RAM owned by the caller
/// until [`dma_unmap_sg`]; no other mapping may alias the same extents
/// concurrently.
///
/// # Errors
/// `AllocError::InvalidInput` for an already-mapped or zero-extent table,
/// `AllocError::NoMemory` when the bounce pool or bookkeeping cannot satisfy
/// the request; the table stays unmapped on error.
///
/// # Panics
/// Panics if the table is already mapped (caller contract violation).
pub unsafe fn dma_map_sg(
    table: &mut SgTable,
    constraints: &DmaConstraints,
    direction: DmaDirection,
) -> AllocResult<usize> {
    if table.nents != 0 {
        return Err(alloc_engine::AllocError::InvalidInput);
    }
    let count = table.segs.len();
    let mut state = Vec::new();
    let mut rollback: Vec<MapState> = Vec::new();

    let result = (|| -> AllocResult<()> {
        for index in 0..count {
            let can_direct = can_map_direct(&table.segs[index], constraints);
            if can_direct {
                map_direct(&mut table.segs[index]);
                // Map-time maintenance: push CPU data for device reads, and
                // clean+invalidate for device writes so a stale dirty line
                // cannot land after the device's data.
                let invalidate = matches!(direction, DmaDirection::DeviceToDriver);
                // SAFETY: the caller's contract guarantees live owned RAM.
                unsafe { sync_direct(&table.segs[index], invalidate) };
                state.push(MapState {
                    path: MapPath::Direct,
                });
                rollback.push(MapState {
                    path: MapPath::Direct,
                });
                continue;
            }

            // Bounce path: stage the extent through the coherent pool. The
            // linear map gives CPU access to the source pages.
            let entry = &table.segs[index];
            let len = entry.input_len;
            let vaddr = p2v(entry.page_paddr);
            // SAFETY: the linear map makes the extent CPU-addressable and
            // non-null.
            let slice = unsafe {
                NonNull::new_unchecked(core::ptr::slice_from_raw_parts_mut(vaddr.as_mut_ptr(), len))
            };
            // SAFETY: the caller guarantees exclusive ownership until unmap.
            let info = unsafe { ALLOCATOR.lock().map_dma_buffer(slice, direction) }?;
            table.segs[index].bus_addr = info.bus_addr;
            table.segs[index].len = len;
            rollback.push(MapState {
                path: MapPath::Bounced,
            });
            state.push(MapState {
                path: MapPath::Bounced,
            });
        }
        Ok(())
    })();

    if let Err(error) = result {
        // Roll back every staged mapping so a failed map leaves no pool
        // occupancy behind.
        for (index, staged) in rollback.iter().enumerate() {
            if staged.path == MapPath::Bounced {
                let len = table.segs[index].input_len;
                let bus = table.segs[index].bus_addr;
                // SAFETY: this mapping was staged in this call and never
                // published to a device.
                unsafe { ALLOCATOR.lock().cancel_dma_mapping(bus, len) };
            }
        }
        table.segs.iter_mut().for_each(|segment| {
            segment.bus_addr = DmaBusAddress::default();
            segment.len = segment.input_len;
        });
        return Err(error);
    }

    table.state = state;
    merge_segments(table, constraints);
    table.nents = table.segs.len();
    Ok(table.nents)
}

/// Merges physically contiguous direct segments while the device limits
/// allow, shrinking the segment list in place. Bounced segments never merge:
/// each owns one pool allocation.
fn merge_segments(table: &mut SgTable, constraints: &DmaConstraints) {
    let mut write = 0usize;
    for read in 1..table.segs.len() {
        let merged = {
            let prev = &table.segs[write];
            let next = &table.segs[read];
            let prev_state = table.state[write];
            let next_state = table.state[read];
            prev_state.path == MapPath::Direct
                && next_state.path == MapPath::Direct
                && prev.page_paddr.as_usize() + prev.input_len == next.page_paddr.as_usize()
                && prev.len + next.len <= constraints.max_segment_size
                && !constraints.crosses_boundary(prev.bus_addr.as_u64(), prev.len + next.len)
        };
        if merged {
            table.segs[write].len += table.segs[read].len;
        } else {
            write += 1;
            table.segs.swap(write, read);
            table.state.swap(write, read);
        }
    }
    table.segs.truncate(write + 1);
    table.state.truncate(write + 1);
}

/// Unmaps a previously mapped SG table and synchronizes data back.
///
/// Bounced extents copy their pool contents back for device-to-CPU
/// directions; direct extents on non-coherent platforms get a clean +
/// invalidate so the CPU reads device results from DRAM.
///
/// # Safety
///
/// The table must be mapped exactly once by [`dma_map_sg`] with the same
/// direction, and the device must have stopped using every bus address.
pub unsafe fn dma_unmap_sg(table: &mut SgTable, direction: DmaDirection) {
    if table.nents == 0 {
        return;
    }
    for index in 0..table.segs.len() {
        let state = table.state[index];
        match state.path {
            MapPath::Bounced => {
                let len = table.segs[index].len;
                let bus = table.segs[index].bus_addr;
                let entry = &table.segs[index];
                let vaddr = p2v(entry.page_paddr);
                // SAFETY: the linear map makes the extent CPU-addressable
                // and non-null.
                let slice = unsafe {
                    NonNull::new_unchecked(core::ptr::slice_from_raw_parts_mut(
                        vaddr.as_mut_ptr(),
                        len,
                    ))
                };
                // SAFETY: the exact mapping from dma_map_sg, unmapped once.
                unsafe { ALLOCATOR.lock().unmap_dma_buffer(bus, slice, direction) };
            }
            MapPath::Direct => {
                // Post-device maintenance: drop stale lines so the CPU reads
                // device-written data from DRAM (no-op for TO_DEVICE where
                // the CPU data is already current).
                if direction != DmaDirection::DriverToDevice {
                    // SAFETY: live owned RAM per the mapping contract.
                    unsafe { sync_direct(&table.segs[index], true) };
                }
            }
        }
    }
    table.segs.iter_mut().for_each(|segment| {
        segment.bus_addr = DmaBusAddress::default();
        segment.len = segment.input_len;
    });
    table.nents = 0;
    table.state.clear();
}

/// Synchronizes one mapped SG table for continued CPU or device access.
///
/// Call `for_cpu` after the device finished writing and before the CPU
/// reads; call `for_device` after the CPU wrote and before restarting the
/// device. Bounced segments need nothing (the coherent pool stays coherent).
///
/// # Safety
///
/// The table must be mapped, and the device must be quiesced across the
/// transition the direction describes.
pub unsafe fn dma_sync_sg(table: &SgTable, direction: DmaDirection, for_cpu: bool) {
    if table.nents == 0 {
        return;
    }
    for index in 0..table.nents {
        if table.state[index].path != MapPath::Direct {
            continue;
        }
        // TO_DEVICE restarts need a clean; CPU reads need clean+invalidate.
        // Direct FROM_DEVICE data the CPU already consumed needs no action.
        let invalidate = for_cpu;
        let clean_needed = !for_cpu || direction != DmaDirection::DeviceToDriver;
        if !clean_needed && !invalidate {
            continue;
        }
        // SAFETY: live owned RAM per the mapping contract.
        unsafe { sync_direct(&table.segs[index], invalidate) };
    }
}

/// Maps one single extent for streaming DMA (the `dma_map_page` form).
///
/// Returns the bus address and the mapped length (equal to `len`).
///
/// # Safety
///
/// Same contract as [`dma_map_sg`] applied to one extent; pair with
/// [`dma_unmap_page`].
///
/// # Errors
/// Same fallback policy as [`dma_map_sg`] for one extent.
pub unsafe fn dma_map_page(
    paddr: PhysAddr,
    len: usize,
    constraints: &DmaConstraints,
    direction: DmaDirection,
) -> AllocResult<DmaBusAddress> {
    let mut table = SgTable::from_entries(&[SgEntry::new(paddr, len)])?;
    // SAFETY: forwards the single-extent mapping contract.
    let nents = unsafe { dma_map_sg(&mut table, constraints, direction) }?;
    debug_assert_eq!(nents, 1);
    Ok(table.segs[0].bus_addr)
}

/// Unmaps one extent mapped by [`dma_map_page`].
///
/// # Safety
///
/// The extent must still be mapped exactly once with the same direction and
/// constraints, and the device must have stopped using the bus address.
pub unsafe fn dma_unmap_page(
    paddr: PhysAddr,
    len: usize,
    bus_addr: DmaBusAddress,
    constraints: &DmaConstraints,
    direction: DmaDirection,
) {
    // Rebuild the minimal table and run the standard unmap so bounce
    // bookkeeping and cache maintenance follow one code path.
    let mut table = match SgTable::from_entries(&[SgEntry::new(paddr, len)]) {
        Ok(table) => table,
        Err(_) => return,
    };
    let direct = can_map_direct(&table.segs[0], constraints);
    table.segs[0].bus_addr = bus_addr;
    table.segs[0].len = len;
    table.nents = 1;
    table.state.push(MapState {
        path: if direct {
            MapPath::Direct
        } else {
            MapPath::Bounced
        },
    });
    // SAFETY: forwards the single-extent unmap contract.
    unsafe { dma_unmap_sg(&mut table, direction) };
}

unittest::path_mod_test!(tests, "../ut-tests/sg.rs");
