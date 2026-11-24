// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IOMMU core framework: isolation domains, IOVA allocation and hardware binding.
//!
//! The crate is the middle layer between the DMA subsystem and IOMMU hardware
//! drivers:
//!
//! ```text
//! [ DMA Subsystem / dma_map_sg ]
//!              │
//!              ▼
//!      ┌──────────────┐   manages   ┌────────────────┐
//!      │ IommuDomain  │────────────▶│ IovaAllocator  │
//!      └──────┬───────┘             └────────────────┘
//!             │ owns                     ▲
//!             ▼                          │ registers
//!      ┌──────────────┐            ┌─────┴──────────┐
//!      │ IoPageTable  │            │ IommuHardware  │ (SMMU-500 / RK-IOMMU)
//!      └──────────────┘            └────────────────┘
//! ```
//!
//! # Ownership model
//!
//! * An [`IommuHardware`] implementation is registered by its driver during
//!   probe ([`register_instance`]) and stays registered until driver removal.
//! * A hardware instance owns one default [`IommuDomain`] (the kernel DMA
//!   domain). Devices attach with [`attach_device`]; the driver programs its
//!   stream matcher / context through [`IommuHardware::attach`].
//! * Consumers translate addresses through [`domain_for_device`] and the
//!   domain's [`IommuDomain::map_pages`] / [`IommuDomain::unmap_pages`], which
//!   pair IOVA allocation with page-table updates and call the hardware TLB
//!   maintenance hook.
//!
//! # Execution context
//!
//! Domain operations serialize on a per-domain spin lock (`SpinNoIrq`): DMA
//! mapping happens in driver hot paths that may hold device locks. Hardware
//! hooks must therefore be IRQ-safe. Domain creation and device attachment run
//! in task context only.
//!
//! # Fault reporting
//!
//! Hardware drivers deliver faults as [`IommuFault`] values through
//! [`report_fault`]; the core owns the observable log format so both the
//! SMMU-500 and Rockchip paths print the same device/IOVA/cause shape.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::fmt;

pub use io_pgtable::{ArmLpae, ArmLpaeConfig, IommuProt, Iova, RockchipV2};
use io_pgtable::{IoPageTable, PgTableError};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

/// Why an IOMMU operation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IommuError {
    /// No instance or domain matches the identifier.
    NotFound,
    /// The device is already attached to this instance.
    AlreadyAttached,
    /// The device is not attached to this instance.
    NotAttached,
    /// The hardware rejected the binding (no SMR/context/stream slot).
    HardwareBusy,
    /// The IOVA space could not satisfy the request.
    NoIovaSpace,
    /// The page-table backend rejected the request.
    PageTable(PgTableError),
}

impl fmt::Display for IommuError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IommuError::NotFound => formatter.write_str("no such IOMMU instance or device"),
            IommuError::AlreadyAttached => formatter.write_str("device already attached"),
            IommuError::NotAttached => formatter.write_str("device not attached"),
            IommuError::HardwareBusy => formatter.write_str("hardware binding slot exhausted"),
            IommuError::NoIovaSpace => formatter.write_str("IOVA space exhausted"),
            IommuError::PageTable(error) => write!(formatter, "page table: {error}"),
        }
    }
}

impl From<PgTableError> for IommuError {
    fn from(error: PgTableError) -> Self {
        IommuError::PageTable(error)
    }
}

/// Identity of one IOMMU hardware instance, supplied by its driver.
///
/// Drivers derive it from their firmware node so that device `iommus`
/// references resolve to the probed instance.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InstanceId(pub u64);

/// Stable identity of one DMA master attached to a domain.
///
/// The host kernel derives it from its device object id; the core treats it
/// as an opaque key.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceKey(pub u64);

/// One device's binding to an IOMMU instance, as parsed from firmware.
///
/// `args` carries the firmware cells after the instance reference (the port /
/// Stream ID for `#iommu-cells = 1`, empty for `#iommu-cells = 0`).
#[derive(Clone, Debug)]
pub struct IommuBinding {
    pub instance: InstanceId,
    pub args: Vec<u32>,
}

/// Hardware-neutral operations one IOMMU driver implements.
///
/// Called from the core with the instance's registration lock held; all
/// methods must be IRQ-safe because domain mapping runs under the domain spin
/// lock.
pub trait IommuHardware: Send + Sync {
    /// Binds one master (Stream ID / port from its firmware cells) to
    /// `domain`'s page-table root.
    ///
    /// # Errors
    /// [`IommuError::HardwareBusy`] when no matcher/context slot is free.
    fn attach(&self, master: &[u32], domain: &IommuDomain) -> Result<(), IommuError>;

    /// Releases one master's binding.
    fn detach(&self, master: &[u32]) -> Result<(), IommuError>;

    /// Invalidates the whole IOTLB of this instance after page-table
    /// maintenance, waiting for completion.
    fn flush_iotlb_all(&self);
}

/// Power-of-two IOVA allocator backed by a bitmap.
///
/// Tracks free pages at the backend's base page size over the window the
/// domain chooses to manage (the bitmap costs one bit per page; a 4 GiB
/// window at 4 KiB pages is 128 KiB of bitmap). Larger allocations consume
/// consecutive aligned runs so a device requiring IOVA contiguity gets
/// naturally aligned windows.
pub struct IovaAllocator {
    /// One bit per base page: `1` = in use.
    bitmap: SpinNoIrq<BitmapState>,
    base_page: u64,
    total_pages: usize,
}

struct BitmapState {
    words: Vec<u64>,
    used: usize,
}

impl IovaAllocator {
    /// Creates an allocator over `window_bytes` in `base_page` granules.
    /// IOVA 0 is reserved (a zero bus address is ambiguous for drivers),
    /// matching common DMA-API practice.
    pub fn new(window_bytes: u64, base_page: u64) -> Self {
        let total_pages = (window_bytes / base_page) as usize;
        let words = total_pages.div_ceil(64);
        Self {
            bitmap: SpinNoIrq::new(BitmapState {
                words: alloc::vec![0; words],
                used: 0,
            }),
            base_page,
            total_pages,
        }
    }

    /// Base page size in bytes.
    pub const fn base_page_size(&self) -> u64 {
        self.base_page
    }

    /// Bytes of the address space this allocator manages.
    pub const fn window_bytes(&self) -> u64 {
        (self.total_pages as u64).saturating_mul(self.base_page)
    }

    /// Currently allocated bytes (diagnostics).
    pub fn used_bytes(&self) -> u64 {
        self.bitmap.lock().used as u64 * self.base_page
    }

    /// Allocates `size` bytes aligned to `align` (both powers of two of the
    /// base page) and returns the IOVA.
    ///
    /// # Errors
    /// [`IommuError::NoIovaSpace`] when no aligned run fits.
    pub fn allocate(&self, size: u64, align: u64) -> Result<Iova, IommuError> {
        if size == 0 || !size.is_power_of_two() || !align.is_power_of_two() {
            return Err(IommuError::NoIovaSpace);
        }
        let pages = size.div_ceil(self.base_page) as usize;
        let align_pages = (align / self.base_page).max(1) as usize;
        if pages > self.total_pages {
            return Err(IommuError::NoIovaSpace);
        }
        let mut state = self.bitmap.lock();
        // First-fit scan over words. The space is scanned from index 1 so the
        // zero IOVA stays reserved.
        let words = state.words.len();
        let mut run_start = 1usize;
        while run_start + pages <= self.total_pages.min(words * 64) {
            // Align the candidate.
            let aligned = (run_start + align_pages - 1) & !(align_pages - 1);
            if aligned + pages > self.total_pages {
                break;
            }
            if range_free(&state.words, aligned, pages) {
                mark_range(&mut state.words, aligned, pages, true);
                state.used += pages;
                return Ok(Iova::new(aligned as u64 * self.base_page));
            }
            // Skip past this candidate's first occupied page.
            run_start = aligned + first_used_in(&state.words, aligned, pages) + 1;
        }
        Err(IommuError::NoIovaSpace)
    }

    /// Returns a previously allocated range to the free pool.
    pub fn free(&self, iova: Iova, size: u64) {
        let start = (iova.as_u64() / self.base_page) as usize;
        let pages = size.div_ceil(self.base_page) as usize;
        let mut state = self.bitmap.lock();
        if start + pages <= state.words.len() * 64 {
            mark_range(&mut state.words, start, pages, false);
            state.used = state.used.saturating_sub(pages);
        }
    }
}

fn range_free(words: &[u64], start: usize, pages: usize) -> bool {
    for index in start..start + pages {
        if words[index / 64] >> (index % 64) & 1 != 0 {
            return false;
        }
    }
    true
}

fn first_used_in(words: &[u64], start: usize, pages: usize) -> usize {
    for (offset, index) in (start..start + pages).enumerate() {
        if words[index / 64] >> (index % 64) & 1 != 0 {
            return offset;
        }
    }
    pages - 1
}

fn mark_range(words: &mut [u64], start: usize, pages: usize, used: bool) {
    for index in start..start + pages {
        let bit = 1u64 << (index % 64);
        if used {
            words[index / 64] |= bit;
        } else {
            words[index / 64] &= !bit;
        }
    }
}

/// One isolation domain: a page table plus its IOVA space.
pub struct IommuDomain {
    page_table: SpinNoIrq<Box<dyn IoPageTable>>,
    iova: IovaAllocator,
    hardware: SpinNoIrq<Vec<Arc<dyn IommuHardware>>>,
}

impl IommuDomain {
    /// Creates a domain over `page_table`, managing `window_bytes` of IOVA
    /// space in `base_page` granules.
    pub fn new(page_table: Box<dyn IoPageTable>, window_bytes: u64, base_page: u64) -> Self {
        Self {
            page_table: SpinNoIrq::new(page_table),
            iova: IovaAllocator::new(window_bytes, base_page),
            hardware: SpinNoIrq::new(Vec::new()),
        }
    }

    /// Physical address of the page-table root for hardware programming.
    pub fn root_table_address(&self) -> u64 {
        self.page_table.lock().root_table_address()
    }

    /// Maps `size` bytes at one physical address into a fresh IOVA window and
    /// flushes the hardware TLB.
    ///
    /// `align` lets callers request IOVA alignment stricter than the page
    /// size (for hardware with address-bit constraints).
    ///
    /// # Errors
    /// Propagates IOVA allocation and page-table errors; a failed map
    /// allocates nothing.
    pub fn map(
        &self,
        paddr: u64,
        size: u64,
        prot: IommuProt,
        align: u64,
    ) -> Result<Iova, IommuError> {
        let mut page_table = self.page_table.lock();
        let pgsize = page_table.pgsize_bitmap();
        let base_page = pgsize.trailing_zeros();
        if size == 0 {
            return Err(IommuError::PageTable(PgTableError::UnsupportedPageSize));
        }
        let pages = size.div_ceil(1 << base_page);
        let pgsize_bytes = 1u64 << base_page;
        let align = if align > pgsize_bytes {
            align
        } else {
            pgsize_bytes
        };
        let iova = self.iova.allocate(size, align)?;
        let mapped = page_table
            .map_pages(iova, paddr, pgsize_bytes, pages as usize, prot)
            .map_err(|error| {
                self.iova.free(iova, size);
                IommuError::PageTable(error)
            })?;
        if mapped != pages as usize {
            // Partial map: roll the placed range back.
            let _ = page_table.unmap_pages(iova, pgsize_bytes, mapped);
            self.iova.free(iova, size);
            return Err(IommuError::NoIovaSpace);
        }
        for hardware in self.hardware.lock().iter() {
            hardware.flush_iotlb_all();
        }
        Ok(iova)
    }

    /// Maps a scatter of (physical, length) segments, merging physically
    /// contiguous segments into one IOVA run when the page table supports
    /// arbitrary page counts.
    ///
    /// Returns the IOVA of the first segment; subsequent segments are placed
    /// at `iova + cumulative_offset`, so consumers can translate each segment
    /// with [`IommuDomain::translate`].
    ///
    /// # Errors
    /// Any failure unmaps everything this call placed.
    pub fn map_segments(
        &self,
        segments: &[(u64, u64)],
        prot: IommuProt,
    ) -> Result<Iova, IommuError> {
        let total: u64 = segments.iter().map(|(_, len)| len).sum();
        if total == 0 {
            return Err(IommuError::PageTable(PgTableError::UnsupportedPageSize));
        }
        let mut page_table = self.page_table.lock();
        let pgsize_bytes = page_table.pgsize_bitmap() & (!page_table.pgsize_bitmap() + 1);
        let pages = total.div_ceil(pgsize_bytes);
        let iova = self.iova.allocate(total, pgsize_bytes)?;
        let mut placed = 0u64;
        for (paddr, len) in segments {
            if *len == 0 {
                continue;
            }
            let count = len.div_ceil(pgsize_bytes);
            // Physically contiguous segments still get one page descriptor
            // each; the IOVA run keeps them contiguous for the device.
            let segment_iova = Iova::new(iova.as_u64() + placed);
            let mapped = page_table
                .map_pages(segment_iova, *paddr, pgsize_bytes, count as usize, prot)
                .map_err(|error| {
                    self.rollback(&mut page_table, iova, placed);
                    self.iova.free(iova, total);
                    IommuError::PageTable(error)
                })?;
            placed += mapped as u64 * pgsize_bytes;
            if mapped as u64 != count {
                self.rollback(&mut page_table, iova, placed);
                self.iova.free(iova, total);
                return Err(IommuError::NoIovaSpace);
            }
        }
        let _ = pages;
        for hardware in self.hardware.lock().iter() {
            hardware.flush_iotlb_all();
        }
        Ok(iova)
    }

    fn rollback(&self, page_table: &mut Box<dyn IoPageTable>, iova: Iova, placed: u64) {
        if placed > 0 {
            let pgsize_bytes = page_table.pgsize_bitmap() & (!page_table.pgsize_bitmap() + 1);
            let _ = page_table.unmap_pages(iova, pgsize_bytes, (placed / pgsize_bytes) as usize);
        }
    }

    /// Unmaps a range previously returned by [`IommuDomain::map`] and flushes
    /// the hardware TLB before the IOVA is reusable.
    pub fn unmap(&self, iova: Iova, size: u64) -> Result<(), IommuError> {
        let mut page_table = self.page_table.lock();
        let pgsize_bytes = page_table.pgsize_bitmap() & (!page_table.pgsize_bitmap() + 1);
        let pages = size.div_ceil(pgsize_bytes);
        page_table
            .unmap_pages(iova, pgsize_bytes, pages as usize)
            .map_err(IommuError::PageTable)?;
        for hardware in self.hardware.lock().iter() {
            hardware.flush_iotlb_all();
        }
        // Only now may the IOVA space hand these pages out again.
        self.iova.free(iova, size);
        Ok(())
    }

    /// Read-back translation for diagnostics and fault handling.
    pub fn translate(&self, iova: Iova) -> Option<u64> {
        self.page_table.lock().translate(iova)
    }

    /// IOVA allocator statistics (diagnostics).
    pub fn iova_window_bytes(&self) -> u64 {
        self.iova.window_bytes()
    }

    /// Stage-1 register surface for SMMU-style hardware.
    ///
    /// Returns `(tcr, mair0)`; `(0, 0)` when the backend has no stage-1
    /// surface (the caller must not program a context bank from zeros).
    pub fn stage1_registers(&self) -> (u64, u64) {
        match self.page_table.lock().stage1_config() {
            Some(config) => (config.tcr, config.mair0),
            None => (0, 0),
        }
    }

    /// Registers one hardware instance with this domain so its TLB
    /// maintenance runs on every map/unmap.
    fn add_hardware(&self, hardware: Arc<dyn IommuHardware>) {
        self.hardware.lock().push(hardware);
    }
}

struct InstanceRecord {
    id: InstanceId,
    hardware: Arc<dyn IommuHardware>,
    /// The instance's kernel DMA domain.
    domain: Arc<IommuDomain>,
    /// Devices currently attached, for detach and fault attribution.
    attached: Vec<(DeviceKey, Vec<u32>)>,
}

static INSTANCES: LazyInit<kspin::SpinNoPreempt<Vec<InstanceRecord>>> = LazyInit::new();

fn instances() -> &'static kspin::SpinNoPreempt<Vec<InstanceRecord>> {
    INSTANCES.call_once(|| kspin::SpinNoPreempt::new(Vec::new()));
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once permanently published the \
                  instance registry"
    )]
    INSTANCES.get().expect("instance registry is initialized")
}

/// Registers one probed IOMMU hardware instance with its DMA domain.
///
/// The driver builds the domain (page-table backend of its choice plus IOVA
/// geometry) and passes it together with the hardware ops. Registration is
/// idempotent per [`InstanceId`]: a re-probe returns the existing domain
/// without registering hardware twice.
pub fn register_instance(
    id: InstanceId,
    hardware: Arc<dyn IommuHardware>,
    domain: Arc<IommuDomain>,
) -> Arc<IommuDomain> {
    let mut records = instances().lock();
    if let Some(existing) = records.iter().find(|record| record.id == id) {
        return existing.domain.clone();
    }
    domain.add_hardware(hardware.clone());
    records.push(InstanceRecord {
        id,
        hardware,
        domain: domain.clone(),
        attached: Vec::new(),
    });
    domain
}

/// Attaches one device to the instance that its firmware binding names.
///
/// # Errors
/// [`IommuError::NotFound`] when the instance is not probed yet (the caller —
/// usually a late attach pass after enumeration — may retry),
/// [`IommuError::HardwareBusy`] when the hardware has no free slot.
pub fn attach_device(
    instance: InstanceId,
    device: DeviceKey,
    master: &[u32],
) -> Result<(), IommuError> {
    let mut records = instances().lock();
    let Some(record) = records.iter_mut().find(|record| record.id == instance) else {
        return Err(IommuError::NotFound);
    };
    if record.attached.iter().any(|(key, _)| *key == device) {
        return Err(IommuError::AlreadyAttached);
    }
    record.hardware.attach(master, &record.domain)?;
    record.attached.push((device, master.to_vec()));
    Ok(())
}

/// Detaches a device and releases its hardware binding.
pub fn detach_device(instance: InstanceId, device: DeviceKey) -> Result<(), IommuError> {
    let mut records = instances().lock();
    let Some(record) = records.iter_mut().find(|record| record.id == instance) else {
        return Err(IommuError::NotFound);
    };
    let Some(position) = record.attached.iter().position(|(key, _)| *key == device) else {
        return Err(IommuError::NotAttached);
    };
    let (_, master) = record.attached.swap_remove(position);
    record.hardware.detach(&master)
}

/// Snapshot of one registered IOMMU instance, for system bring-up checks.
#[derive(Clone)]
pub struct InstanceSnapshot {
    pub id: InstanceId,
    pub domain: Arc<IommuDomain>,
}

/// Returns every registered instance with its domain.
pub fn registered_instances() -> Vec<InstanceSnapshot> {
    instances()
        .lock()
        .iter()
        .map(|record| InstanceSnapshot {
            id: record.id,
            domain: record.domain.clone(),
        })
        .collect()
}

/// Returns the domain a device translates through, if attached.
pub fn domain_for_device(device: DeviceKey) -> Option<Arc<IommuDomain>> {
    instances()
        .lock()
        .iter()
        .find(|record| record.attached.iter().any(|(key, _)| *key == device))
        .map(|record| record.domain.clone())
}

/// One translation fault, normalized across hardware backends.
#[derive(Clone, Copy, Debug)]
pub struct IommuFault {
    /// Instance that reported the fault.
    pub instance: InstanceId,
    /// Master identifier from the hardware fault registers (Stream ID or port).
    pub master: u32,
    /// Faulting IOVA.
    pub iova: Iova,
    /// The transaction was a write.
    pub is_write: bool,
}

impl fmt::Display for IommuFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "IOMMU fault on instance {:?}: master {} {} at iova {:#x}",
            self.instance,
            self.master,
            if self.is_write { "write" } else { "read" },
            self.iova.as_u64()
        )
    }
}

/// Logs one fault with the shared observable format.
///
/// Hardware drivers call this from their ISR after reading and **clearing**
/// their fault status registers; the core never touches hardware.
pub fn report_fault(fault: &IommuFault) {
    log::error!("{fault}; master must be attached to a domain before its device starts DMA");
}

/// Clears the instance registry. Unit tests only.
#[cfg(unittest)]
pub fn reset_for_tests() {
    instances().lock().clear();
}

unittest::path_mod_test!(tests, "../ut-tests/core.rs");
