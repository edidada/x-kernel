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
use kspin::{SpinNoIrq, static_lock};
use log::error;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr, va};

use crate::{
    DmaBusAddress, DmaCacheIf, DmaCachePolicy, DmaDeviceContext, DmaDirection, DmaInfo,
    DmaMappingKind, DmaMappingPolicy, DmaSyncTarget,
    bounce_pool::{BouncePool, DMA_BOUNCE_POOL_PAGES, bounce_pool_size},
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

    /// Query a complete physically contiguous RAM range with uniform attributes.
    /// Return None for inaccessible, non-RAM, discontinuous or platform-private
    /// memory. The caller retains the range and its stable attributes until unmap.
    fn query_contiguous(vaddr: VirtAddr, size: usize) -> Option<(PhysAddr, MappingFlags)>;
}

static_lock! {
    static ALLOCATOR: SpinNoIrq<DmaAllocator> = SpinNoIrq::new(DmaAllocator::new());
}

pub(crate) fn lock_allocator() -> kspin::SpinNoIrqGuard<'static, DmaAllocator> {
    loop {
        if let Some(guard) = ALLOCATOR.try_lock() {
            return guard;
        }
        // The owner can be waiting for this IRQ-off CPU to acknowledge a page-attribute change.
        DmaPageTableIf::poll_pending_tlb();
        core::hint::spin_loop();
    }
}

pub(crate) struct DmaAllocator {
    bounce_pool: Option<BouncePool>,
    active_mappings: BTreeMap<u64, ActiveMapping>,
    #[cfg(feature = "dma-trace")]
    coherent_allocs: BTreeMap<usize, CoherentTrace>,
}

/// Mapping bookkeeping owns staging storage; callers own and pin original RAM.
struct ActiveMapping {
    original_cpu_addr: usize,
    len: usize,
    direction: DmaDirection,
    owner: DmaSyncTarget,
    backing: MappingBacking,
}

enum MappingBacking {
    Direct { cache_policy: DmaCachePolicy },
    Bounce { cpu_addr: usize, layout: Layout },
}

impl ActiveMapping {
    fn kind(&self) -> DmaMappingKind {
        match self.backing {
            MappingBacking::Direct { .. } => DmaMappingKind::Direct,
            MappingBacking::Bounce { .. } => DmaMappingKind::Bounce,
        }
    }

    /// Synchronize before changing owner; an unsuccessful hook leaves owner unchanged.
    /// # Safety
    /// The original buffer is live, pinned and exclusive, and the previous owner
    /// has stopped all accesses. The record owns any distinct bounce allocation.
    unsafe fn sync(&mut self, target: DmaSyncTarget) -> AllocResult<()> {
        let maintain = |start, len, operation| {
            // SAFETY: caller establishes live exclusive whole-cache-line RAM,
            // stable attributes and stopped owners until maintenance completes.
            unsafe { crate::cache::maintain(start, len, operation) }
        };
        // SAFETY: caller establishes live exclusive RAM and stopped owners;
        // the callback completes maintenance without retaining the range.
        unsafe { self.sync_with(target, maintain) }
    }

    /// # Safety
    /// Same pinned-memory and stopped-owner contract as sync. The callback must
    /// complete requested maintenance synchronously or return an error.
    unsafe fn sync_with(
        &mut self,
        target: DmaSyncTarget,
        mut maintain: impl FnMut(VirtAddr, usize, crate::DmaCacheOperation) -> AllocResult<()>,
    ) -> AllocResult<()> {
        use core::sync::atomic::{Ordering, fence};
        if self.owner == target {
            return Ok(());
        }
        // Cache coherence and bounce copies do not replace device-publication ordering.
        fence(Ordering::SeqCst);
        match self.backing {
            MappingBacking::Direct {
                cache_policy: DmaCachePolicy::NonCoherent { .. },
            } => {
                if let Some(operation) = crate::cache::operation(self.direction, target) {
                    maintain(va!(self.original_cpu_addr), self.len, operation)?;
                }
            }
            MappingBacking::Bounce { cpu_addr, .. } => {
                let pair = match target {
                    DmaSyncTarget::Device if self.direction != DmaDirection::DeviceToDriver => {
                        Some((self.original_cpu_addr, cpu_addr))
                    }
                    DmaSyncTarget::Cpu if self.direction != DmaDirection::DriverToDevice => {
                        Some((cpu_addr, self.original_cpu_addr))
                    }
                    _ => None,
                };
                if let Some((source, destination)) = pair {
                    // SAFETY: record retains distinct initialized regions covering len;
                    // caller has stopped the previous owner and keeps original RAM live.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            source as *const u8,
                            destination as *mut u8,
                            self.len,
                        )
                    };
                }
            }
            MappingBacking::Direct { .. } => {}
        }
        fence(Ordering::SeqCst);
        self.owner = target;
        Ok(())
    }
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
struct CoherentTrace {
    bus_addr: DmaBusAddress,
    num_pages: usize,
    site: TraceSite,
}

impl DmaAllocator {
    pub(crate) const fn new() -> Self {
        Self {
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
    pub(crate) unsafe fn allocate_dma_memory(
        &mut self,
        device: &DmaDeviceContext,
        layout: Layout,
    ) -> AllocResult<DmaInfo> {
        let dma = self.alloc_coherent_pages(layout)?;
        let checked = device
            .check_range(
                dma.bus_addr.as_u64(),
                layout_pages(&layout) * PAGE_SIZE_4K,
                false,
            )
            .and_then(|()| {
                if dma.bus_addr.as_u64().is_multiple_of(layout.align() as u64) {
                    Ok(())
                } else {
                    Err(AllocError::InvalidInput)
                }
            });
        if let Err(error) = checked {
            // SAFETY: this allocation retains its original layout and has never been exposed to hardware.
            unsafe { self.deallocate_dma_memory(dma, layout) };
            return Err(error);
        }
        Ok(dma)
    }

    #[track_caller]
    fn alloc_coherent_pages(&mut self, layout: Layout) -> AllocResult<DmaInfo> {
        if layout.size() == 0 {
            return Err(AllocError::InvalidInput);
        }
        let num_pages = layout_pages(&layout);
        let vaddr_raw = global_allocator().alloc_dma_pages(
            num_pages,
            PAGE_SIZE_4K.max(layout.align()),
            UsageKind::Dma,
        )?;
        let vaddr = va!(vaddr_raw);
        let flags = MappingFlags::READ
            | MappingFlags::WRITE
            | MappingFlags::UNCACHED
            | MappingFlags::SHARED;
        let bus_addr = (v2p(vaddr).as_usize() as u64)
            .checked_add(kbuild_config::PHYS_BUS_OFFSET as u64)
            .filter(|start| {
                start
                    .checked_add((num_pages * PAGE_SIZE_4K - 1) as u64)
                    .is_some()
            });
        let Some(bus_addr) = bus_addr else {
            global_allocator().dealloc_dma_pages(vaddr_raw, num_pages, UsageKind::Dma);
            return Err(AllocError::InvalidInput);
        };
        let bus_addr = DmaBusAddress::new(bus_addr);
        // Initialize padding while the allocation still has its original cache
        // attributes; maintenance below preserves these bytes during the transition.
        // SAFETY: vaddr_raw identifies fresh exclusive writable pages, and the full
        // rounded extent is owned by this allocation and unpublished to hardware.
        unsafe { (vaddr_raw as *mut u8).write_bytes(0, num_pages * PAGE_SIZE_4K) };
        // SAFETY: zeroed whole allocated pages remain exclusive; hardware and
        // prior allocation aliases have no access during the cache transition.
        unsafe { crate::cache::prepare_attribute_change(vaddr, num_pages * PAGE_SIZE_4K) }?;
        // A failed update can leave uncertain cache/share attributes. Quarantine
        // these pages rather than return them to the general allocator.
        // SAFETY: freshly allocated whole pages remain exclusive and unpublished;
        // aliases from the prior allocation have retired before allocator reuse.
        unsafe { self.update_flags(vaddr, num_pages, flags) }?;
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

    /// # Safety
    /// Whole allocated pages are live and exclusive with all aliases and hardware
    /// accesses retired while cache and mapping attributes transition.
    unsafe fn update_flags(
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

        // Complete maintenance while the existing shared mapping is still valid;
        // a platform unshare can revoke that access before page flags are restored.
        // SAFETY: caller stopped hardware and retired CPU aliases; the original
        // allocation/layout retains the entire exclusive live page extent.
        if unsafe { crate::cache::prepare_attribute_change(vaddr, size) }.is_err() {
            return;
        }
        if self.release_platform_dma(v2p(vaddr), size).is_err() {
            return;
        }
        // SAFETY: caller stopped hardware and retired aliases; the original
        // allocation/layout identifies the complete exclusively owned live pages.
        if unsafe { self.update_flags(vaddr, num_pages, MappingFlags::READ | MappingFlags::WRITE) }
            .is_err()
        {
            return;
        }

        global_allocator().dealloc_dma_pages(virt_raw, num_pages, UsageKind::Dma);
    }

    /// Select direct or coherent staging storage for a temporary DMA transaction.
    /// # Safety
    /// Buffer is initialized pinned kernel RAM, live with stable attributes and
    /// exclusive ownership through unmap, including every alias and cache line.
    pub(crate) unsafe fn map_dma_buffer(
        &mut self,
        device: &DmaDeviceContext,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> AllocResult<DmaInfo> {
        // SAFETY: caller retains the initialized, pinned, exclusive range through unmap.
        unsafe { self.map_with_policy(device, buffer, direction, DmaMappingPolicy::Auto) }
            .map(|(info, _)| info)
    }

    /// Map with an explicit backend-selection policy.
    /// # Safety
    /// Same original-buffer and ownership requirements as map_dma_buffer.
    pub(crate) unsafe fn map_with_policy(
        &mut self,
        device: &DmaDeviceContext,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
        policy: DmaMappingPolicy,
    ) -> AllocResult<(DmaInfo, DmaMappingKind)> {
        let len = buffer.len();
        let original_cpu_addr = buffer.cast::<u8>().as_ptr() as usize;
        if len == 0 || original_cpu_addr.checked_add(len).is_none() {
            return Err(AllocError::InvalidInput);
        }
        let direct = if policy == DmaMappingPolicy::Bounce {
            None
        } else {
            DmaPageTableIf::query_contiguous(va!(original_cpu_addr), len).and_then(
                |(paddr, flags)| {
                    direct_candidate(
                        device,
                        original_cpu_addr,
                        len,
                        paddr,
                        flags,
                        DmaCacheIf::policy(),
                    )
                },
            )
        };
        let (dma, backing) = match direct {
            Some((bus_addr, cache_policy)) => (
                DmaInfo {
                    cpu_addr: buffer.cast(),
                    bus_addr,
                },
                MappingBacking::Direct { cache_policy },
            ),
            None if policy == DmaMappingPolicy::Direct => return Err(AllocError::InvalidInput),
            None => {
                let (dma, layout) = self.alloc_bounce_buffer(len)?;
                if let Err(error) = device.check_range(dma.bus_addr.as_u64(), len, true) {
                    self.recycle_bounce_buffer(dma.cpu_addr.as_ptr() as usize, layout);
                    return Err(error);
                }
                if direction == DmaDirection::DeviceToDriver {
                    // Preserve receive bytes on cancellation and short device writes;
                    // other directions copy during the initial Device handoff below.
                    // SAFETY: caller retains initialized original bytes; the new bounce
                    // allocation is distinct, exclusive and covers exactly len bytes.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            buffer.cast::<u8>().as_ptr(),
                            dma.cpu_addr.as_ptr(),
                            len,
                        )
                    };
                }
                (
                    dma,
                    MappingBacking::Bounce {
                        cpu_addr: dma.cpu_addr.as_ptr() as usize,
                        layout,
                    },
                )
            }
        };
        let mut mapping = ActiveMapping {
            original_cpu_addr,
            len,
            direction,
            owner: DmaSyncTarget::Cpu,
            backing,
        };
        if self.overlaps_mapping(dma.bus_addr.as_u64(), len) {
            self.recycle_mapping(mapping);
            return Err(AllocError::InvalidInput);
        }
        // SAFETY: preparation retains the original exclusive range and staging
        // allocation, and hardware has not received the bus address yet.
        if let Err(error) = unsafe { mapping.sync(DmaSyncTarget::Device) } {
            self.recycle_mapping(mapping);
            return Err(error);
        }
        let kind = mapping.kind();
        self.active_mappings.insert(dma.bus_addr.as_u64(), mapping);
        Ok((dma, kind))
    }

    fn overlaps_mapping(&self, start: u64, len: usize) -> bool {
        // Every published mapping was checked for nonempty, nonoverflowing bounds.
        let end = start + len as u64 - 1;
        let previous = self.active_mappings.range(..=start).next_back();
        let next = self.active_mappings.range(start..).next();
        previous.is_some_and(|(base, mapping)| start - base < mapping.len as u64)
            || next.is_some_and(|(base, _)| *base <= end)
    }

    fn recycle_mapping(&mut self, mapping: ActiveMapping) {
        if let MappingBacking::Bounce { cpu_addr, layout } = mapping.backing {
            self.recycle_bounce_buffer(cpu_addr, layout);
        }
    }

    /// Synchronize to CPU ownership and retire a matching active mapping.
    /// # Safety
    /// These are the original mapping arguments; hardware and CPU accesses have
    /// stopped, and the original pinned range remains live and exclusive.
    pub(crate) unsafe fn try_unmap_dma_buffer(
        &mut self,
        dma_addr: DmaBusAddress,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> AllocResult<()> {
        // SAFETY: caller supplies the original pinned range after both owners stop.
        unsafe { self.sync_dma_buffer(dma_addr, buffer, direction, DmaSyncTarget::Cpu) }?;
        if let Some(mapping) = self.active_mappings.remove(&dma_addr.as_u64()) {
            self.recycle_mapping(mapping);
        }
        Ok(())
    }

    /// Legacy unmap contract: a caller violation or failed handoff retains memory.
    /// # Safety
    /// Original mapping arguments, exact-once release and stopped hardware are required.
    pub(crate) unsafe fn unmap_dma_buffer(
        &mut self,
        dma_addr: DmaBusAddress,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) {
        // SAFETY: the legacy caller supplies the original exclusive range after DMA stops.
        let result = unsafe { self.try_unmap_dma_buffer(dma_addr, buffer, direction) };
        assert!(
            result.is_ok(),
            "DMA unmap must complete for the original active mapping"
        );
    }

    /// Synchronize an active mapping without releasing its address or storage.
    /// # Safety
    /// Caller owns the original pinned range and has stopped the previous owner's
    /// accesses before requesting an ownership transition.
    pub(crate) unsafe fn sync_dma_buffer(
        &mut self,
        addr: DmaBusAddress,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
        target: DmaSyncTarget,
    ) -> AllocResult<()> {
        let mapping = self
            .active_mappings
            .get_mut(&addr.as_u64())
            .ok_or(AllocError::InvalidInput)?;
        if mapping.original_cpu_addr != buffer.cast::<u8>().as_ptr() as usize
            || mapping.len != buffer.len()
            || mapping.direction != direction
        {
            return Err(AllocError::InvalidInput);
        }
        // SAFETY: argument validation matched the retained original record; caller
        // keeps that range live and exclusive and stopped the previous owner.
        unsafe { mapping.sync(target) }
    }

    fn alloc_bounce_buffer(&mut self, len: usize) -> AllocResult<(DmaInfo, Layout)> {
        self.bounce_pool_mut()?.allocate(len)
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
            let dma = self.alloc_coherent_pages(layout_for_pages(DMA_BOUNCE_POOL_PAGES))?;
            self.bounce_pool = Some(BouncePool::new(dma, bounce_pool_size()));
        }
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: the None branch installs the bounce \
                      pool or returns the allocation error before reaching this expression"
        )]
        Ok(self.bounce_pool.as_mut().unwrap())
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
            CoherentTrace {
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

/// Pure eligibility check after the host establishes physical continuity and RAM provenance.
fn direct_candidate(
    device: &DmaDeviceContext,
    cpu_addr: usize,
    len: usize,
    paddr: PhysAddr,
    flags: MappingFlags,
    policy: DmaCachePolicy,
) -> Option<(DmaBusAddress, DmaCachePolicy)> {
    if !flags.contains(MappingFlags::READ | MappingFlags::WRITE)
        || flags.intersects(MappingFlags::DEVICE | MappingFlags::USER)
    {
        return None;
    }
    let policy = if flags.contains(MappingFlags::UNCACHED) {
        DmaCachePolicy::Coherent
    } else {
        policy
    };
    if !crate::cache::can_direct(policy, cpu_addr, len) {
        return None;
    }
    let bus = (paddr.as_usize() as u64).checked_add(kbuild_config::PHYS_BUS_OFFSET as u64)?;
    device.check_range(bus, len, true).ok()?;
    Some((DmaBusAddress::new(bus), policy))
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
    layout.size().div_ceil(PAGE_SIZE_4K)
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

#[cfg(unittest)]
#[path = "tests.rs"]
mod tests;
