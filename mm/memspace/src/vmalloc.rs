// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! General-purpose virtually-contiguous kernel memory allocator.
//!
//! This is the X-Kernel counterpart of Linux `mm/vmalloc.c`:
//!
//! - Virtual-address bookkeeping lives in a dedicated interval table
//!   ([`VmallocState::areas`]) over `[VMALLOC_BASE, VMALLOC_END)`, separate
//!   from user/kernel address-space VMAs (Linux `vmap_area` tree).
//! - Page-table updates run in a single `PageTableMut` transaction so TLB
//!   flushes are deferred and coalesced (Linux `map_vm_area` + one flush).
//! - `free` is callable from hardirq/softirq context: such frees are
//!   queued on a lock-free intrusive list and drained by `system_wq`
//!   (Linux `vfree_deferred`). Task-context frees are synchronous.
//!
//! # Locking
//!
//! Lock order is `VMALLOC` → kernel address-space lock ([`kernel_layout`]);
//! no other code path takes them in reverse order. Both are `SpinNoIrq`, and
//! the lock holders **never** run the synchronous TLB shootdown:
//! page-table commits use [`page_table::PageTableMut::finish_no_flush`], and
//! only `free` flushes — locally via `karch::flush_tlb` and remotely via
//! [`kipi::tlb::flush_all_cpus`] — after releasing the locks. This avoids
//! deadlocking when a target CPU is spinning on one of these locks with IRQs
//! disabled and thus cannot service the shootdown IPI. `alloc` needs no
//! flush: a fresh VA has no stale TLB entries, and a freed VA stays reserved
//! in the interval table until the preceding `free`'s system-wide flush has
//! completed, so a reused VA cannot have stale entries on any CPU. The
//! deferred-free list is lock-free (Treiber stack) so it can be touched from
//! IRQ context.

use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use kaddr_layout::{VMALLOC_BASE, VMALLOC_END};
use kalloc::global_allocator;
use kerrno::{KError, KResult};
use khal::{mem::v2p, paging::MappingFlags};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memaddr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use page_table::PageSize;

use crate::{aspace::MmSpace, kernel_layout};

/// An allocated virtual region. The caller owns it until it is passed back
/// to [`free`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmRegion {
    /// Base of the whole region, including any guard region.
    pub va: VirtAddr,
    /// Total size of the region, including any guard region.
    pub size: usize,
}

/// Allocation parameters for [`alloc_with`].
#[derive(Debug, Clone, Copy)]
pub struct VmAllocOptions {
    /// Total size of the allocation, including the guard region. Must be a
    /// multiple of 4 KiB.
    pub size: usize,
    /// VA alignment; must be a power of two of at least 4 KiB.
    pub align: usize,
    /// Mapping flags applied to every page.
    pub flags: MappingFlags,
    /// Number of unmapped guard pages at the base of the region.
    pub guard_pages: usize,
}

// ── VA interval bookkeeping (Linux vmap_area) ─────────────────────────

struct VmallocState {
    /// Allocated intervals: start VA → size. Kept disjoint and sorted.
    areas: BTreeMap<usize, usize>,
}

const fn empty_state() -> VmallocState {
    VmallocState {
        areas: BTreeMap::new(),
    }
}

static VMALLOC: SpinNoIrq<VmallocState> = SpinNoIrq::new(empty_state());

fn vmalloc_range() -> VirtAddrRange {
    let start = VirtAddr::from_usize(VMALLOC_BASE);
    let end = VirtAddr::from_usize(VMALLOC_END);
    VirtAddrRange::from_start_size(start, end.as_usize().saturating_sub(VMALLOC_BASE))
}

impl VmallocState {
    /// Finds a free interval of `size` bytes inside `limit`, aligned to
    /// `align`, scanning the allocated intervals in ascending order.
    fn find_free_area(&self, limit: VirtAddrRange, size: usize, align: usize) -> Option<VirtAddr> {
        if size == 0 || !size.is_multiple_of(align) {
            return None;
        }
        let mut last_end = limit.start.align_up(align);
        for (&start, &area_size) in self.areas.range(last_end.as_usize()..) {
            let start = VirtAddr::from_usize(start).max(last_end);
            if start.as_usize().saturating_sub(last_end.as_usize()) >= size {
                return Some(last_end);
            }
            last_end = VirtAddr::from_usize(start.as_usize() + area_size).align_up(align);
        }
        if limit.end.as_usize().saturating_sub(last_end.as_usize()) >= size {
            Some(last_end)
        } else {
            None
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────

/// Allocates a virtually-contiguous region with default options: 4 KiB
/// alignment, read-write pages, and `guard_pages` unmapped guard pages.
pub fn alloc(size: usize, guard_pages: usize) -> KResult<(VmRegion, Vec<usize>)> {
    alloc_with(VmAllocOptions {
        size,
        align: PAGE_SIZE_4K,
        flags: MappingFlags::READ | MappingFlags::WRITE,
        guard_pages,
    })
}

/// Allocates a virtually-contiguous region according to [`VmAllocOptions`].
///
/// On success the caller receives the region handle and the physical-page
/// handles (`dm_va`) backing the usable range, in order.
pub fn alloc_with(opts: VmAllocOptions) -> KResult<(VmRegion, Vec<usize>)> {
    let guard = opts.guard_pages * PAGE_SIZE_4K;
    if !opts.align.is_power_of_two() {
        return Err(KError::InvalidInput);
    }
    let align = opts.align.max(PAGE_SIZE_4K);
    if opts.size <= guard {
        return Err(KError::InvalidInput);
    }
    let usable = opts.size - guard;
    if !usable.is_multiple_of(PAGE_SIZE_4K)
        || !opts.size.is_multiple_of(PAGE_SIZE_4K)
        || !opts.size.is_multiple_of(align)
    {
        return Err(KError::InvalidInput);
    }
    let nr_pages = usable / PAGE_SIZE_4K;
    let limit = vmalloc_range();

    // Pre-allocate the physical pages outside the vmalloc and kernel
    // address-space locks: `map_pages` then only touches page tables while
    // holding the two `SpinNoIrq` locks, keeping IRQs-off time bounded.
    let mut dm_vas = Vec::with_capacity(nr_pages);
    for _ in 0..nr_pages {
        match global_allocator().alloc_pages(1, PAGE_SIZE_4K, kalloc::UsageKind::VirtMem) {
            Ok(v) => dm_vas.push(v),
            Err(_) => {
                for v in dm_vas.drain(..) {
                    global_allocator().dealloc_pages(v, 1, kalloc::UsageKind::VirtMem);
                }
                return Err(KError::NoMemory);
            }
        }
    }

    let mut vstate = VMALLOC.lock();
    let Some(va) = vstate.find_free_area(limit, opts.size, align) else {
        drop(vstate);
        for v in dm_vas {
            global_allocator().dealloc_pages(v, 1, kalloc::UsageKind::VirtMem);
        }
        return Err(KError::NoMemory);
    };

    let mut aspace = kernel_layout().lock();
    let map_result = map_pages(&mut aspace, va, guard, opts.flags, &dm_vas);
    if let Err(e) = map_result {
        drop(aspace);
        drop(vstate);
        for dm_va in dm_vas {
            global_allocator().dealloc_pages(dm_va, 1, kalloc::UsageKind::VirtMem);
        }
        return Err(e);
    }
    drop(aspace);

    vstate.areas.insert(va.as_usize(), opts.size);
    drop(vstate);

    // No TLB flush here: a freshly allocated VA has no stale TLB entries, and
    // a reused VA is published back to the interval table by `free_sync` only
    // after its system-wide flush (local `karch::flush_tlb` + remote
    // `flush_all_cpus`) has completed, so no CPU can still hold a stale entry
    // for it.
    Ok((
        VmRegion {
            va,
            size: opts.size,
        },
        dm_vas,
    ))
}

/// Frees a region previously returned by [`alloc`]/[`alloc_with`].
///
/// Safe to call from hardirq/softirq and task context: in exception context
/// the teardown is queued on the lock-free deferred list and drained by
/// `system_wq`. NMI context is not supported — the workerqueue does not
/// promise NMI safety, so NMI handlers must not release kernel stacks.
pub fn free(region: VmRegion, guard_pages: usize, pages: Vec<usize>) {
    if kcpu::in_exception_context() {
        free_deferred(region, guard_pages, pages);
    } else {
        free_sync(region, guard_pages, pages);
    }
}

// ── Mapping helpers ───────────────────────────────────────────────────

/// Maps the pre-allocated pages in `dm_vas` at `va + guard` in a single
/// page-table transaction. On failure, the already-mapped prefix is unmapped
/// inside the same transaction and every page stays in `dm_vas` so the caller
/// can return them to the allocator.
fn map_pages(
    aspace: &mut MmSpace,
    va: VirtAddr,
    guard: usize,
    flags: MappingFlags,
    dm_vas: &[usize],
) -> KResult<()> {
    let mut modify = aspace.page_table_mut().modify();
    for (i, &dm_va) in dm_vas.iter().enumerate() {
        let page_va = va + guard + i * PAGE_SIZE_4K;
        let pa = v2p(VirtAddr::from_usize(dm_va));
        if modify.map(page_va, pa, PageSize::Size4K, flags).is_err() {
            warn!("vmalloc: map {page_va:?} failed; unwinding {i} pages");
            if i > 0 {
                let _ = modify.unmap_region(va + guard, i * PAGE_SIZE_4K);
            }
            // Do not let the `PageTableMut` Drop run the synchronous TLB
            // shootdown while the caller still holds the `SpinNoIrq` locks:
            // the freshly mapped pages were never mapped before, so no flush
            // is needed.
            modify.finish_no_flush();
            return Err(KError::NoMemory);
        }
    }
    modify.finish_no_flush();
    Ok(())
}

fn free_sync(region: VmRegion, guard_pages: usize, pages: Vec<usize>) {
    let guard = guard_pages * PAGE_SIZE_4K;
    let usable = region.size.saturating_sub(guard);
    let usable_start = region.va + guard;

    // Keep this VA reserved while tearing down its PTEs. The entry remains in
    // the interval table until the subsequent TLB shootdown has completed.
    let vstate = VMALLOC.lock();
    let mut need_flush = false;
    if usable > 0 {
        let mut aspace = kernel_layout().lock();
        let mut modify = aspace.page_table_mut().modify();
        if let Err(e) = modify.unmap_region(usable_start, usable) {
            // The page table no longer matches the interval bookkeeping — an
            // unrecoverable vmalloc bug. Stop here instead of freeing the VA
            // while stale mappings may still reference the pages.
            panic!(
                "vmalloc: unmap {:?} size {:#x} failed: {:?}; page-table state corrupt",
                usable_start, usable, e
            );
        }
        // Commit the PTE teardown now, but flush the TLB only after releasing
        // the locks: a synchronous shootdown while holding `SpinNoIrq` locks
        // deadlocks a CPU that is spinning on them with IRQs disabled.
        modify.finish_no_flush();
        drop(modify);
        drop(aspace);
        need_flush = true;
    }
    drop(vstate);

    if need_flush {
        // The kernel page table is shared by every CPU, so the unmap must be
        // reflected in every CPU's TLB — including this one. The remote
        // broadcast skips the initiator, so flush locally first; the local
        // flush is unconditional (not gated on `ALL_CPUS_STARTED`), which
        // also covers early-boot frees. Both flushes run outside the
        // `VMALLOC` / `KERNEL_ASPACE` locks: a synchronous shootdown while
        // holding `SpinNoIrq` locks deadlocks a CPU spinning on them with
        // IRQs off.
        karch::flush_tlb(None);
        kipi::tlb::flush_all_cpus(None);
    }

    // Do not make this VA available again until every CPU has discarded the
    // previous mapping. Holding `VMALLOC` across the shootdown could deadlock
    // a target CPU that is waiting on this lock with IRQs disabled, so release
    // the interval only after the lock-free flush has completed.
    VMALLOC.lock().areas.remove(&region.va.as_usize());

    for dm_va in pages {
        global_allocator().dealloc_pages(dm_va, 1, kalloc::UsageKind::VirtMem);
    }
}

// ── Deferred free (Linux vfree_deferred) ──────────────────────────────
//
// A fixed pool of nodes is linked into a lock-free intrusive list, so queueing
// from IRQ context never allocates and never takes a spinlock. `system_wq`
// drains the list and returns the nodes to the pool.

const DEFERRED_POOL_SIZE: usize = 64;

struct PendingFreeNode {
    next: AtomicPtr<PendingFreeNode>,
    region: VmRegion,
    guard_pages: usize,
    pages: Vec<usize>,
}

const fn empty_node() -> PendingFreeNode {
    PendingFreeNode {
        next: AtomicPtr::new(core::ptr::null_mut()),
        region: VmRegion {
            va: VirtAddr::from_usize(0),
            size: 0,
        },
        guard_pages: 0,
        pages: Vec::new(),
    }
}

static NODES: [PendingFreeNode; DEFERRED_POOL_SIZE] = [const { empty_node() }; DEFERRED_POOL_SIZE];
static FREE_HEAD: AtomicPtr<PendingFreeNode> = AtomicPtr::new(core::ptr::null_mut());
static RELEASE_HEAD: AtomicPtr<PendingFreeNode> = AtomicPtr::new(core::ptr::null_mut());
static DEFERRED_WORK: LazyInit<kirq::workerqueue::WorkItem> = LazyInit::new();

/// Set when a drain attempt was rejected (e.g. `QueueFull`) so the next push
/// or drain retries instead of leaving nodes parked on the list.
static NEED_SCHEDULE: AtomicBool = AtomicBool::new(false);

/// Prepares the deferred-free pool and registers the drain work item.
/// Called once at boot from task context.
pub fn init() {
    for node in &NODES {
        // SAFETY: boot is single-threaded and the nodes are not yet on any
        // list; each node is exclusively owned by the free pool afterwards.
        llist_add(node as *const _ as *mut _, &FREE_HEAD);
    }
    DEFERRED_WORK.init_once(kirq::workerqueue::WorkItem::new(|_work| {
        deferred_drain();
    }));
}

/// Adds `node` to the head of `head`; returns true if the list was empty.
fn llist_add(node: *mut PendingFreeNode, head: &AtomicPtr<PendingFreeNode>) -> bool {
    let mut cur = head.load(Ordering::Relaxed);
    loop {
        // SAFETY: `node` is exclusively owned by the caller and not on any
        // list; the `next` slot is not read concurrently while we write it.
        unsafe { (*node).next.store(cur, Ordering::Relaxed) };
        match head.compare_exchange_weak(cur, node, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => return cur.is_null(),
            Err(actual) => cur = actual,
        }
    }
}

/// Detaches and returns the whole list.
fn llist_pop_all(head: &AtomicPtr<PendingFreeNode>) -> *mut PendingFreeNode {
    head.swap(core::ptr::null_mut(), Ordering::Acquire)
}

/// Takes one node from the free pool, or panics if the pool is exhausted.
///
/// This is a Treiber-stack pop, which is in principle ABA-prone. It is safe
/// here because the only producers of `FREE_HEAD` are one-time `init()` and
/// the serialized `system_wq` drain worker: between a consumer's head load
/// and its CAS, a popped node cannot complete the full
/// release → drain → return round trip that an ABA retry would require.
fn pop_free_node() -> *mut PendingFreeNode {
    loop {
        let head = FREE_HEAD.load(Ordering::Relaxed);
        if head.is_null() {
            panic!("vmalloc: deferred-free pool exhausted");
        }
        // SAFETY: `head` points to a node owned by the free pool.
        let next = unsafe { (*head).next.load(Ordering::Relaxed) };
        if FREE_HEAD
            .compare_exchange_weak(head, next, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return head;
        }
    }
}

fn schedule_drain() {
    let work = DEFERRED_WORK
        .get()
        .expect("vmalloc: deferred work not initialized");
    let result = kirq::workerqueue::schedule_work(work);
    match result {
        kirq::workerqueue::QueueWorkResult::Queued
        | kirq::workerqueue::QueueWorkResult::AlreadyQueued => {}
        other => {
            // The fixed queue is full or disabled; retry on the next push or
            // at the end of the next drain instead of leaving nodes parked.
            NEED_SCHEDULE.store(true, Ordering::Release);
            warn!("vmalloc: deferred drain not queued ({other:?}); will retry");
        }
    }
}

/// Queues a region for deferred teardown. Callable from hardirq/softirq
/// context; NMI is not supported (the workerqueue takes spinlocks).
fn free_deferred(region: VmRegion, guard_pages: usize, pages: Vec<usize>) {
    let node = pop_free_node();
    // SAFETY: the node was just removed from the free pool and is exclusively
    // owned here; its previous contents (empty `pages`) are dropped.
    unsafe {
        (*node).region = region;
        (*node).guard_pages = guard_pages;
        (*node).pages = pages;
    }
    if llist_add(node, &RELEASE_HEAD) || NEED_SCHEDULE.swap(false, Ordering::AcqRel) {
        schedule_drain();
    }
}

/// Drains all queued frees. Runs on `system_wq` (task context).
fn deferred_drain() {
    loop {
        let head = llist_pop_all(&RELEASE_HEAD);
        if head.is_null() {
            break;
        }
        let mut node = head;
        while !node.is_null() {
            // SAFETY: this worker is the only consumer of the detached list;
            // nodes are exclusively owned while they are being drained.
            let (next, region, guard_pages, pages) = unsafe {
                let next = (*node).next.load(Ordering::Relaxed);
                let region = (*node).region;
                let guard_pages = (*node).guard_pages;
                let pages = core::mem::take(&mut (*node).pages);
                (next, region, guard_pages, pages)
            };
            free_sync(region, guard_pages, pages);
            // SAFETY: the node is idle again and exclusively owned.
            llist_add(node, &FREE_HEAD);
            node = next;
        }
    }
    // Retry a previously rejected drain attempt.
    if NEED_SCHEDULE.swap(false, Ordering::AcqRel) {
        schedule_drain();
    }
}

#[cfg(all(unittest, feature = "vmap_stack"))]
mod tests_vmalloc {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn guard_hole_is_unmapped_usable_is_mapped() {
        let (region, pages) = alloc(0x4000, 1).expect("vmalloc alloc");
        let guard_start = region.va;
        let usable_start = region.va + memaddr::PAGE_SIZE_4K;
        {
            let aspace = kernel_layout().lock();
            assert!(
                aspace.page_table().query(guard_start).is_err(),
                "guard page must be an unmapped VA hole"
            );
            assert!(
                aspace.page_table().query(usable_start).is_ok(),
                "first usable page must be mapped"
            );
        }
        free(region, 1, pages);
    }
}
