// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::collections::btree_map::BTreeMap;
#[cfg(feature = "dma-trace")]
use core::panic::Location;
use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::{AllocError, AllocResult};
use kalloc::{PageRef, UsageKind, alloc_dma_pages_ref};
use khal::mem::{p2v, v2p};
use kiface_mm_api::{DmaMappingIf, DmaMappingMode};
use kspin::{SpinNoIrq, static_lock};
use log::error;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr, va};

use crate::{
    DmaBusAddress, DmaInfo,
    bounce_pool::{BounceMapping, BouncePool, DMA_BOUNCE_POOL_PAGES, bounce_pool_size},
    p2b,
};

static_lock! {
    pub(crate) static ALLOCATOR: SpinNoIrq<DmaAllocator> = SpinNoIrq::new(DmaAllocator::new());
}

pub(crate) struct DmaAllocator {
    bounce_pool: BouncePool,
    active_mappings: BTreeMap<u64, BounceMapping>,
    #[cfg(feature = "dma-trace")]
    coherent_allocs: BTreeMap<usize, CoherentAllocation>,
    /// Test-only in-use coherent allocations, keyed by CPU address.
    #[cfg(unittest)]
    test_coherent_allocs: BTreeMap<usize, usize>,
}

/// Read-only snapshot of in-use DMA resources, for unit-test baselines.
#[cfg(unittest)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DmaTestStats {
    /// Number of active streaming (bounce) mappings.
    pub active_mappings: usize,
    /// Total bytes of active streaming (bounce) mappings.
    pub active_mapping_bytes: usize,
    /// Bounce-pool bytes currently allocated.
    pub bounce_used_bytes: usize,
    /// Bounce-pool total size, or zero before it is initialized.
    pub bounce_total_bytes: usize,
    /// Number of active coherent allocations.
    pub coherent_allocations: usize,
    /// Total bytes of active coherent allocations.
    pub coherent_bytes: usize,
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

fn can_change_dma_mapping() -> bool {
    karch::local_irq_enabled()
        && !kiface_irq_context_api::InterruptContextIf::is_in_interrupt_context()
        && kiface_task_api::TaskBlockingContextIf::is_sleepable_context()
}

/// Allocate arbitrary number of bytes. Returns the left bound of the
/// allocated region.
///
/// DMA memory is allocated in page granularity so page-table attributes and
/// platform share/unshare hooks remain balanced for every allocation.
///
/// The returned allocation is private until its caller publishes it to a device.
#[track_caller]
pub(crate) fn allocate_dma_memory(layout: Layout) -> AllocResult<DmaInfo> {
    alloc_coherent_pages(layout)
}

#[track_caller]
fn alloc_coherent_pages(layout: Layout) -> AllocResult<DmaInfo> {
    if !can_change_dma_mapping() {
        return Err(AllocError::InvalidInput);
    }
    let num_pages = layout_pages(&layout);
    let pages = alloc_dma_pages_ref(num_pages, PAGE_SIZE_4K.max(layout.align()), UsageKind::Dma)?;
    let vaddr = p2v(pages.phys_addr());
    let vaddr_raw = vaddr.as_usize();
    if let Err(error) = update_mapping_mode(vaddr, num_pages, DmaMappingMode::Coherent) {
        // A failed transition may leave uncached/shared PTEs or stale translations.
        // Retain the existing PageRef hold; no independent lifetime protocol exists.
        core::mem::forget(pages);
        return Err(error);
    }
    if let Err(error) = prepare_platform_dma(pages.phys_addr(), num_pages * PAGE_SIZE_4K) {
        core::mem::forget(pages);
        return Err(error);
    }
    // DmaInfo plus the original layout carries one exact PageRef transfer through
    // the legacy driver ABI. The matching release restores this hold once.
    let _page_transfer = pages.into_raw();
    let dma_info = DmaInfo {
        // SAFETY: `vaddr_raw` is the start of the freshly allocated DMA mapping
        // and therefore cannot be null.
        cpu_addr: unsafe { NonNull::new_unchecked(vaddr_raw as *mut u8) },
        bus_addr: v2b(vaddr),
    };
    {
        let mut allocator = ALLOCATOR.lock();
        allocator.trace_coherent_alloc(dma_info, num_pages);
        #[cfg(unittest)]
        allocator.test_coherent_allocs.insert(
            dma_info.cpu_addr.as_ptr() as usize,
            num_pages * PAGE_SIZE_4K,
        );
    }
    Ok(dma_info)
}

fn update_mapping_mode(vaddr: VirtAddr, num_pages: usize, mode: DmaMappingMode) -> AllocResult<()> {
    let expand_size = num_pages * PAGE_SIZE_4K;
    DmaMappingIf::set_mode(vaddr, expand_size, mode).map_err(|_| {
        error!("change table flag fail");
        AllocError::NoMemory
    })
}

fn prepare_platform_dma(paddr: PhysAddr, size: usize) -> AllocResult<()> {
    kplat::dma::prepare(paddr.as_usize(), size).map_err(|_| {
        error!("platform dma prepare failed");
        AllocError::NoMemory
    })
}

fn release_platform_dma(paddr: PhysAddr, size: usize) -> AllocResult<()> {
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
pub(crate) unsafe fn deallocate_dma_memory(dma: DmaInfo, layout: Layout) {
    if !can_change_dma_mapping() {
        error!("coherent DMA release requires sleepable task context; retaining pages");
        return;
    }
    debug_assert!(layout.size() != 0);
    debug_assert!(
        (dma.cpu_addr.as_ptr() as usize).is_multiple_of(PAGE_SIZE_4K.max(layout.align()))
    );
    let num_pages = layout_pages(&layout);
    // Address and geometry are cheap to check. Exact allocation identity follows
    // from the unsafe caller contract; no second owner registry is maintained.
    if !ALLOCATOR.lock().trace_coherent_free(dma, num_pages) {
        return;
    }
    let virt_raw = dma.cpu_addr.as_ptr() as usize;
    let size = num_pages * PAGE_SIZE_4K;
    let vaddr = va!(virt_raw);

    // SAFETY: successful allocation transferred exactly this PageRef range;
    // the caller supplies its original address/layout and releases it once.
    let pages = unsafe { PageRef::from_raw(v2p(vaddr), num_pages) };
    // A failed release must quarantine the existing reference: reusing these
    // pages could expose a new owner to firmware or old mapping attributes.
    if release_platform_dma(v2p(vaddr), size).is_err()
        || update_mapping_mode(vaddr, num_pages, DmaMappingMode::Normal).is_err()
    {
        error!(
            "quarantining DMA pages after failed release: vaddr={:?}, pages={}",
            vaddr, num_pages
        );
        core::mem::forget(pages);
        return;
    }

    #[cfg(unittest)]
    ALLOCATOR.lock().test_coherent_allocs.remove(&virt_raw);
    drop(pages);
}

/// Prepares the shared pool without holding metadata across mapping changes.
pub(crate) fn ensure_bounce_pool() -> AllocResult<()> {
    if !can_change_dma_mapping() {
        return Err(AllocError::InvalidInput);
    }
    if ALLOCATOR.lock().bounce_pool.is_initialized() {
        return Ok(());
    }
    let layout = layout_for_pages(DMA_BOUNCE_POOL_PAGES);
    let dma = alloc_coherent_pages(layout)?;
    let is_installed = {
        let mut allocator = ALLOCATOR.lock();
        if allocator.bounce_pool.is_initialized() {
            false
        } else {
            allocator.bounce_pool.initialize(dma, bounce_pool_size());
            true
        }
    };
    if !is_installed {
        // SAFETY: this candidate was never published or handed to a device,
        // and the layout is exactly the one used for its allocation.
        unsafe { deallocate_dma_memory(dma, layout) };
    }
    Ok(())
}

impl DmaAllocator {
    pub(crate) const fn new() -> Self {
        Self {
            bounce_pool: BouncePool::new(),
            active_mappings: BTreeMap::new(),
            #[cfg(feature = "dma-trace")]
            coherent_allocs: BTreeMap::new(),
            #[cfg(unittest)]
            test_coherent_allocs: BTreeMap::new(),
        }
    }

    /// Returns a read-only snapshot of in-use DMA resources for test baselines.
    #[cfg(unittest)]
    pub(crate) fn test_stats(&self) -> DmaTestStats {
        let (bounce_used_bytes, bounce_total_bytes) = self.bounce_pool.test_used_total_bytes();
        DmaTestStats {
            active_mappings: self.active_mappings.len(),
            active_mapping_bytes: self.active_mappings.values().map(BounceMapping::len).sum(),
            bounce_used_bytes,
            bounce_total_bytes,
            coherent_allocations: self.test_coherent_allocs.len(),
            coherent_bytes: self.test_coherent_allocs.values().sum(),
        }
    }

    /// Reserves exclusive staging storage without publishing an active mapping.
    pub(crate) fn reserve_mapping(&mut self, len: usize) -> AllocResult<(DmaInfo, BounceMapping)> {
        if len == 0 {
            return Err(AllocError::InvalidInput);
        }
        let (dma_info, mapping) = self.bounce_pool.allocate(len)?;
        if self
            .active_mappings
            .contains_key(&dma_info.bus_addr.as_u64())
        {
            error!("DMA mapping is already active: {:?}", dma_info.bus_addr);
            self.recycle_bounce_buffer(mapping);
            return Err(AllocError::InvalidInput);
        }
        Ok((dma_info, mapping))
    }

    /// Publishes a reserved mapping after staging has finished outside the lock.
    pub(crate) fn activate_mapping(&mut self, dma_addr: DmaBusAddress, mapping: BounceMapping) {
        // TLSF cannot return this extent again before its token is recycled.
        let previous = self.active_mappings.insert(dma_addr.as_u64(), mapping);
        debug_assert!(previous.is_none());
    }

    /// Releases a bounced mapping that the device never received.
    ///
    /// The mapping must be unpublished: no device may have accessed its bus
    /// address. This performs no copy-back,
    /// so rollback cannot overwrite the original buffer with stale or
    /// uninitialized bounce contents.
    ///
    /// # Safety
    ///
    /// `dma_addr` must be the still-active mapping returned by
    /// [`crate::map_dma_buffer`] and must be cancelled exactly once. The device
    /// must never have been given the address.
    pub(crate) unsafe fn cancel_dma_mapping(&mut self, dma_addr: DmaBusAddress, len: usize) {
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: The Safety contract requires dma_addr \
                      to be the still-active mapping from map_dma_buffer, cancelled exactly once"
        )]
        let mapping = self
            .active_mappings
            .remove(&dma_addr.as_u64())
            .unwrap_or_else(|| panic!("DMA mapping is not active: {:?}", dma_addr));

        assert_eq!(
            len,
            mapping.len(),
            "DMA buffer length mismatch for {:?}: expected {}, got {}",
            dma_addr,
            mapping.len(),
            len
        );

        self.recycle_bounce_buffer(mapping);
    }

    /// Removes a live mapping before its caller synchronizes and recycles it.
    pub(crate) fn take_mapping(&mut self, dma_addr: DmaBusAddress, len: usize) -> BounceMapping {
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
            len,
            mapping.len(),
            "DMA buffer length mismatch for {:?}: expected {}, got {}",
            dma_addr,
            mapping.len(),
            len
        );

        mapping
    }

    pub(crate) fn recycle_bounce_buffer(&mut self, mapping: BounceMapping) {
        self.bounce_pool.deallocate(mapping);
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

fn v2b(addr: VirtAddr) -> DmaBusAddress {
    let paddr = v2p(addr);
    p2b(paddr)
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
