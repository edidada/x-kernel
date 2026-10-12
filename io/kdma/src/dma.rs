// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::collections::btree_map::BTreeMap;
#[cfg(feature = "dma-trace")]
use core::panic::Location;
use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::{AllocError, AllocResult};
use kalloc::{UsageKind, global_allocator};
use khal::{mem::v2p, paging::MappingFlags};
use log::error;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr, va};

use crate::{
    DmaBusAddress, DmaConstraints, DmaDirection, DmaInfo, DmaWindow,
    bounce_pool::{BounceMapping, BouncePool, DMA_BOUNCE_POOL_PAGES, bounce_pool_size},
};

/// Host hooks for DMA page attributes and TLB progress during lock contention.
/// The host runtime supplies these hooks without a kdma dependency on memspace or IPI internals.
#[kiface::interface]
pub trait DmaPageTableIf {
    /// Drain incoming TLB requests while a CPU waits for a DMA/page-table lock.
    ///
    /// Implementations must pin the current CPU and exclude IRQ reentry while
    /// servicing invalidations. Only TLB acknowledgements may run here.
    fn poll_pending_tlb();

    /// Update the mapping flags for the given virtual address range.
    fn protect(vaddr: VirtAddr, size: usize, flags: MappingFlags) -> kerrno::KResult;
}

pub(crate) struct DmaAllocator {
    constraints: DmaConstraints,
    window: DmaWindow,
    bounce_pool: Option<BouncePool>,
    active_mappings: BTreeMap<u64, BounceMapping>,
    #[cfg(feature = "dma-trace")]
    coherent_allocs: BTreeMap<usize, CoherentAllocation>,
}

#[cfg(feature = "dma-trace")]
#[derive(Clone, Copy)]
struct TraceSite {
    file: &'static str,
    line: u32,
    column: u32,
}

#[cfg(feature = "dma-trace")]
#[derive(Clone, Copy)]
struct CoherentAllocation {
    bus_addr: DmaBusAddress,
    num_pages: usize,
    site: TraceSite,
}

impl DmaAllocator {
    pub(crate) const fn new(constraints: DmaConstraints, window: DmaWindow) -> Self {
        Self {
            constraints,
            window,
            bounce_pool: None,
            active_mappings: BTreeMap::new(),
            #[cfg(feature = "dma-trace")]
            coherent_allocs: BTreeMap::new(),
        }
    }

    /// Allocate arbitrary number of bytes. Returns the left bound of the
    /// allocated region.
    ///
    /// DMA memory is allocated in page granularity so page-table attributes and
    /// platform share/unshare hooks remain balanced for every allocation.
    ///
    /// # Safety
    ///
    /// The caller must treat the returned [`DmaInfo`] as coherent DMA memory:
    /// deallocate it exactly once with the same `layout`, do not forge or
    /// offset the returned CPU address, and only hand the bus address to a
    /// device that is allowed to DMA that range.
    #[track_caller]
    pub(crate) unsafe fn allocate_dma_memory(&mut self, layout: Layout) -> AllocResult<DmaInfo> {
        let dma = self.alloc_coherent_pages(layout, self.constraints.coherent_mask)?;
        let checked = self
            .constraints
            .check_range(dma.bus_addr.as_u64(), layout.size(), false)
            .and_then(|()| {
                if dma.bus_addr.as_u64().is_multiple_of(layout.align() as u64) {
                    Ok(())
                } else {
                    Err(AllocError::InvalidInput)
                }
            });
        if let Err(error) = checked {
            // SAFETY: this allocation has not been exposed to a device and retains its original layout.
            unsafe { self.deallocate_dma_memory(dma, layout) };
            return Err(error);
        }
        Ok(dma)
    }

    #[track_caller]
    fn alloc_coherent_pages(&mut self, layout: Layout, mask: u64) -> AllocResult<DmaInfo> {
        if layout.size() == 0 {
            return Err(AllocError::InvalidInput);
        }
        let num_pages = layout_pages(&layout);
        let (phys_start, phys_end) = self.window.physical_range(mask)?;
        let vaddr_raw = global_allocator().alloc_dma_pages_in_range(
            num_pages,
            PAGE_SIZE_4K
                .max(layout.align())
                .max(self.constraints.min_addr_align_bytes),
            phys_start,
            phys_end,
            UsageKind::Dma,
        )?;
        let vaddr = va!(vaddr_raw);
        let flags = MappingFlags::READ
            | MappingFlags::WRITE
            | MappingFlags::UNCACHED
            | MappingFlags::SHARED;
        let bus_addr = match self
            .window
            .translate(v2p(vaddr).as_usize() as u64, num_pages * PAGE_SIZE_4K)
        {
            Ok(addr) => addr,
            Err(error) => {
                global_allocator().dealloc_dma_pages(vaddr_raw, num_pages, UsageKind::Dma);
                return Err(error);
            }
        };
        // A failed update can leave uncertain cache/share attributes. Quarantine
        // these pages rather than return them to the general allocator.
        self.update_flags(vaddr, num_pages, flags)?;
        self.prepare_platform_dma(v2p(vaddr), num_pages * PAGE_SIZE_4K)?;
        let dma_info = DmaInfo {
            // SAFETY: `vaddr_raw` is the start of the freshly allocated DMA mapping
            // and therefore cannot be null.
            cpu_addr: unsafe { NonNull::new_unchecked(vaddr_raw as *mut u8) },
            bus_addr,
        };
        self.trace_coherent_alloc(dma_info, num_pages);
        Ok(dma_info)
    }

    fn update_flags(
        &mut self,
        vaddr: VirtAddr,
        num_pages: usize,
        flags: MappingFlags,
    ) -> AllocResult<()> {
        let expand_size = num_pages * PAGE_SIZE_4K;
        DmaPageTableIf::protect(vaddr, expand_size, flags).map_err(|_| {
            error!("change table flag fail");
            AllocError::NoMemory
        })
    }

    fn prepare_platform_dma(&mut self, paddr: PhysAddr, size: usize) -> AllocResult<()> {
        kplat::dma::prepare(paddr.as_usize(), size).map_err(|_| {
            error!("platform dma prepare failed");
            AllocError::NoMemory
        })
    }

    fn release_platform_dma(&mut self, paddr: PhysAddr, size: usize) -> AllocResult<()> {
        kplat::dma::release(paddr.as_usize(), size).map_err(|_| {
            error!("platform dma release failed");
            AllocError::NoMemory
        })
    }

    /// Gives back the allocated region to the byte allocator.
    ///
    /// # Safety
    ///
    /// `dma` must come from a prior successful [`allocate_dma_memory`] call on
    /// this allocator, and `layout` must exactly match that allocation.
    #[track_caller]
    pub(crate) unsafe fn deallocate_dma_memory(&mut self, dma: DmaInfo, layout: Layout) {
        let num_pages = layout_pages(&layout);
        if !self.trace_coherent_free(dma, num_pages) {
            return;
        }
        let virt_raw = dma.cpu_addr.as_ptr() as usize;
        let size = num_pages * PAGE_SIZE_4K;
        let vaddr = va!(virt_raw);

        if self.release_platform_dma(v2p(vaddr), size).is_err()
            || self
                .update_flags(vaddr, num_pages, MappingFlags::READ | MappingFlags::WRITE)
                .is_err()
        {
            return;
        }

        global_allocator().dealloc_dma_pages(virt_raw, num_pages, UsageKind::Dma);
    }

    /// Maps an existing CPU buffer through the bounce pool for a temporary DMA transaction.
    ///
    /// # Safety
    ///
    /// `buffer` must contain initialized bytes and remain live and exclusively owned until the
    /// matching [`unmap_dma_buffer`] call. The caller must not let the device
    /// access the returned bus address after unmapping.
    pub(crate) unsafe fn map_dma_buffer(
        &mut self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> AllocResult<DmaInfo> {
        use core::sync::atomic::{Ordering, fence};

        let len = buffer.len();
        if len == 0 || len > self.constraints.max_segment_len_bytes {
            return Err(AllocError::InvalidInput);
        }

        fence(Ordering::SeqCst);
        let (dma_info, layout, allocation_cpu_addr) = self.alloc_bounce_buffer(len)?;

        {
            // Preserve untouched bytes on cancellation and short device writes.
            // SAFETY: the bounce buffer and source slice are distinct live
            // regions of `len` bytes established by the caller and allocator.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    buffer.as_ptr() as *const u8,
                    dma_info.cpu_addr.as_ptr(),
                    len,
                );
            }
            fence(Ordering::SeqCst);
        }

        if self
            .active_mappings
            .contains_key(&dma_info.bus_addr.as_u64())
        {
            error!("DMA mapping is already active: {:?}", dma_info.bus_addr);
            self.recycle_bounce_buffer(allocation_cpu_addr, layout);
            return Err(AllocError::InvalidInput);
        }
        self.active_mappings.insert(
            dma_info.bus_addr.as_u64(),
            BounceMapping {
                allocation_cpu_addr,
                cpu_addr: dma_info.cpu_addr.as_ptr() as usize,
                len,
                layout,
                original_cpu_addr: buffer.as_ptr() as *mut u8 as usize,
                direction,
            },
        );

        Ok(dma_info)
    }

    /// Unmaps a previously bounced DMA buffer and synchronizes data back.
    ///
    /// # Safety
    ///
    /// `dma_addr` must be the still-active mapping returned by
    /// [`map_dma_buffer`] for the same `buffer`, and this function must be
    /// called exactly once for that mapping.
    pub(crate) unsafe fn unmap_dma_buffer(
        &mut self,
        dma_addr: DmaBusAddress,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) {
        use core::sync::atomic::{Ordering, fence};

        let len = buffer.len();
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: The Safety contract requires dma_addr \
                      to be the still-active mapping from map_dma_buffer, unmapped exactly once"
        )]
        let mapping = self
            .active_mappings
            .remove(&dma_addr.as_u64())
            .unwrap_or_else(|| panic!("DMA mapping is not active: {:?}", dma_addr));

        assert_eq!(
            len, mapping.len,
            "DMA buffer length mismatch for {:?}: expected {}, got {}",
            dma_addr, mapping.len, len
        );

        assert_eq!(
            mapping.original_cpu_addr,
            buffer.as_ptr() as *mut u8 as usize,
            "DMA buffer mismatch"
        );
        assert_eq!(mapping.direction, direction, "DMA direction mismatch");
        fence(Ordering::SeqCst);
        if direction != DmaDirection::DriverToDevice {
            // SAFETY: the bounce buffer and destination slice are distinct live
            // regions of `len` bytes tracked by the active mapping record.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    mapping.cpu_addr as *const u8,
                    buffer.as_ptr() as *mut u8,
                    len,
                );
            }
            fence(Ordering::SeqCst);
        }

        self.recycle_bounce_buffer(mapping.allocation_cpu_addr, mapping.layout);
    }

    /// Synchronize an active bounce mapping without releasing its allocation.
    /// # Safety
    /// The caller owns the original buffer and has stopped accesses by the
    /// previous owner before requesting the CPU/device ownership transition.
    pub(crate) unsafe fn sync_dma_buffer(
        &mut self,
        addr: DmaBusAddress,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
        target: crate::DmaSyncTarget,
    ) -> AllocResult<()> {
        use core::sync::atomic::{Ordering, fence};
        let mapping = self
            .active_mappings
            .get(&addr.as_u64())
            .ok_or(AllocError::InvalidInput)?;
        if mapping.original_cpu_addr != buffer.cast::<u8>().as_ptr() as usize
            || mapping.len != buffer.len()
            || mapping.direction != direction
        {
            return Err(AllocError::InvalidInput);
        }
        fence(Ordering::SeqCst);
        let pair = match target {
            crate::DmaSyncTarget::Device if direction != DmaDirection::DeviceToDriver => {
                Some((mapping.original_cpu_addr, mapping.cpu_addr))
            }
            crate::DmaSyncTarget::Cpu if direction != DmaDirection::DriverToDevice => {
                Some((mapping.cpu_addr, mapping.original_cpu_addr))
            }
            _ => None,
        };
        if let Some((source, destination)) = pair {
            // SAFETY: the active record retains the separate original and bounce regions;
            // the caller has stopped the previous owner and both regions cover mapping.len.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source as *const u8,
                    destination as *mut u8,
                    mapping.len,
                )
            };
        }
        fence(Ordering::SeqCst);
        Ok(())
    }

    fn alloc_bounce_buffer(&mut self, len: usize) -> AllocResult<(DmaInfo, Layout, usize)> {
        let constraints = self.constraints;
        let pool = self.bounce_pool_mut()?;
        pool.allocate(len, constraints)
    }

    fn recycle_bounce_buffer(&mut self, cpu_addr: usize, layout: Layout) {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Active bounce mappings retain the \
                      bounce pool until their buffers are recycled"
        )]
        let pool = self
            .bounce_pool
            .as_mut()
            .expect("DMA bounce pool must exist while mappings are active");
        pool.deallocate(cpu_addr, layout);
    }

    fn bounce_pool_mut(&mut self) -> AllocResult<&mut BouncePool> {
        if self.bounce_pool.is_none() {
            let dma = self.alloc_coherent_pages(
                layout_for_pages(DMA_BOUNCE_POOL_PAGES),
                self.constraints.streaming_mask,
            )?;
            self.bounce_pool = Some(BouncePool::new(dma, bounce_pool_size()));
        }
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: the None branch installs the bounce \
                      pool or returns the allocation error before reaching this expression"
        )]
        Ok(self.bounce_pool.as_mut().unwrap())
    }

    pub(crate) fn release_bounce_pool(&mut self) {
        if !self.active_mappings.is_empty() {
            error!("DMA context dropped with active mappings; retaining backing pages");
            return;
        }
        // Retire TLSF bookkeeping before returning its backing pages to the allocator.
        if let Some(backing) = self.bounce_pool.take().map(|pool| pool.backing()) {
            // SAFETY: the pool has no active mappings and owns this original allocation.
            unsafe { self.deallocate_dma_memory(backing, layout_for_pages(DMA_BOUNCE_POOL_PAGES)) };
        }
    }

    #[cfg(feature = "dma-trace")]
    #[track_caller]
    fn trace_coherent_alloc(&mut self, dma: DmaInfo, num_pages: usize) {
        let cpu_addr = dma.cpu_addr.as_ptr() as usize;
        let site = TraceSite::caller();
        if let Some(prev) = self.coherent_allocs.get(&cpu_addr).copied() {
            error!(
                "duplicate coherent DMA allocation record: cpu_addr={:#x}, bus_addr={:#x}, \
                 pages={}, alloc_site={} while previous record bus_addr={:#x}, pages={}, \
                 alloc_site={}",
                cpu_addr,
                dma.bus_addr.as_u64(),
                num_pages,
                site,
                prev.bus_addr.as_u64(),
                prev.num_pages,
                prev.site
            );
            return;
        }
        self.coherent_allocs.insert(
            cpu_addr,
            CoherentAllocation {
                bus_addr: dma.bus_addr,
                num_pages,
                site,
            },
        );
    }

    #[cfg(not(feature = "dma-trace"))]
    fn trace_coherent_alloc(&mut self, _dma: DmaInfo, _num_pages: usize) {}

    #[cfg(feature = "dma-trace")]
    #[track_caller]
    fn trace_coherent_free(&mut self, dma: DmaInfo, num_pages: usize) -> bool {
        let cpu_addr = dma.cpu_addr.as_ptr() as usize;
        let site = TraceSite::caller();

        if let Some(record) = self.coherent_allocs.get(&cpu_addr).copied() {
            if record.bus_addr != dma.bus_addr || record.num_pages != num_pages {
                error!(
                    "coherent DMA free mismatch: cpu_addr={:#x}, bus_addr={:#x}, pages={}, \
                     free_site={}, expected bus_addr={:#x}, pages={}, alloc_site={}; skipping \
                     deallocation",
                    cpu_addr,
                    dma.bus_addr.as_u64(),
                    num_pages,
                    site,
                    record.bus_addr.as_u64(),
                    record.num_pages,
                    record.site
                );
                return false;
            }
            self.coherent_allocs.remove(&cpu_addr);
            return true;
        }

        if let Some((expected_cpu_addr, record)) =
            self.coherent_allocs
                .iter()
                .find_map(|(tracked_cpu_addr, record)| {
                    (record.bus_addr == dma.bus_addr).then_some((*tracked_cpu_addr, *record))
                })
        {
            error!(
                "coherent DMA free used unexpected cpu_addr: cpu_addr={:#x}, bus_addr={:#x}, \
                 pages={}, free_site={}, expected cpu_addr={:#x}, pages={}, alloc_site={}; \
                 skipping deallocation",
                cpu_addr,
                dma.bus_addr.as_u64(),
                num_pages,
                site,
                expected_cpu_addr,
                record.num_pages,
                record.site
            );
            return false;
        }

        error!(
            "coherent DMA free for untracked allocation: cpu_addr={:#x}, bus_addr={:#x}, \
             pages={}, free_site={}; skipping deallocation",
            cpu_addr,
            dma.bus_addr.as_u64(),
            num_pages,
            site
        );
        false
    }

    #[cfg(not(feature = "dma-trace"))]
    fn trace_coherent_free(&mut self, _dma: DmaInfo, _num_pages: usize) -> bool {
        true
    }
}

#[cfg(feature = "dma-trace")]
impl TraceSite {
    #[track_caller]
    fn caller() -> Self {
        let location = Location::caller();
        Self {
            file: location.file(),
            line: location.line(),
            column: location.column(),
        }
    }
}

#[cfg(feature = "dma-trace")]
impl core::fmt::Display for TraceSite {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

const fn layout_pages(layout: &Layout) -> usize {
    memaddr::align_up_4k(layout.size()) / PAGE_SIZE_4K
}

fn layout_for_pages(num_pages: usize) -> Layout {
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: the only caller passes the fixed \
                  DMA_BOUNCE_POOL_PAGES constant; its byte size fits Layout and PAGE_SIZE_4K is a \
                  power of two"
    )]
    Layout::from_size_align(num_pages * PAGE_SIZE_4K, PAGE_SIZE_4K).unwrap()
}
