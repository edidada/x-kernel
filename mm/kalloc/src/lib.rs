// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel global allocator and page allocation helpers.
#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

#[cfg(any(feature = "dice", feature = "tee"))]
mod ffi;

#[cfg(all(feature = "slab", not(feature = "ipi")))]
use core::sync::atomic::AtomicU64;
use core::{
    alloc::{GlobalAlloc, Layout},
    fmt,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicIsize, Ordering},
};

#[cfg(not(feature = "slab"))]
use alloc_engine::ByteAllocator;
use alloc_engine::{AllocError, AllocResult, BaseAllocator, BuddyPageAllocator, PageAllocator};
#[cfg(feature = "slab")]
use alloc_engine::{SizeClass, SlabBacking};
use kaddr_layout::v2p;
#[cfg(feature = "slab")]
use kmem_cache::{
    KMEM_CACHE_FLUSH_SEQUENCE_MAX, KmemCacheSet, TaskMagazineAccess, TaskMagazineRefill,
};
#[cfg(feature = "slab")]
use kspin::NoPreempt;
use kspin::{IrqSave, SpinNoIrq};
#[cfg(feature = "slab")]
use magazine::{ObjectBatch, refill_count};
use memaddr::PAGE_SIZE_4K;
use strum::{IntoStaticStr, VariantArray};

#[cfg(not(feature = "slab"))]
const MIN_HEAP_SIZE: usize = 0x8000; // 32 K

fn pages_to_bytes(num_pages: usize) -> usize {
    num_pages
        .checked_mul(PAGE_SIZE_4K)
        .expect("page count byte size overflow")
}

fn kernel_virt_to_phys(vaddr: usize) -> usize {
    v2p(vaddr)
}

#[cfg(feature = "slab")]
mod kmem_cache;
#[cfg(feature = "slab")]
mod magazine;
mod page;
mod pcp;
pub use page::GlobalPage;

#[cfg(feature = "tracking")]
mod tracking;
#[cfg(feature = "tracking")]
pub use tracking::*;

/// Maximum nonzero generation accepted by the slab magazine flush protocol.
///
/// The IPI provider uses this bound when allocating a sequence for a global
/// reclaim session.  The high bits encode that sequence alongside the local
/// magazine access state, preventing delayed IPIs from re-freezing a CPU after
/// a completed session has released it.
#[cfg(feature = "slab")]
pub const TASK_MAGAZINE_FLUSH_SEQUENCE_MAX: u64 = KMEM_CACHE_FLUSH_SEQUENCE_MAX;

/// Cross-CPU slab magazine reclaim provider.
///
/// `kipi` provides this interface in SMP slab builds.  The provider freezes
/// every present CPU with a no-allocation IPI handshake, drains the frozen
/// magazines from the initiating task context, and later unfreezes them.  A
/// return value of zero means that a session could not be started (for example
/// because another shrink is already active), so callers retain the safe
/// central-only reclaim fallback.
#[cfg(all(feature = "slab", feature = "ipi"))]
#[kiface::interface]
pub trait KmemCacheFlushIf {
    /// Begin one serialized global magazine flush and return its generation.
    fn begin_global_magazine_flush() -> u64;

    /// End a flush previously returned by [`begin_global_magazine_flush`].
    fn end_global_magazine_flush(sequence: u64);
}

// Exactly one allocator feature must be selected. Kernel builds get it
// through kfeat; host test builds pass it explicitly (e.g.
// `cargo test -p kext4 --features kalloc/slab`).
#[cfg(not(any(feature = "slab", feature = "buddy", feature = "tlsf")))]
compile_error!("kalloc requires one of the allocator features: slab, buddy, or tlsf");

#[cfg(not(feature = "slab"))]
cfg_if::cfg_if! {
    if #[cfg(feature = "buddy")] {
        /// The default byte allocator.
        pub type DefaultByteAllocator = alloc_engine::BuddyByteAllocator;
    } else if #[cfg(feature = "tlsf")] {
        /// The default byte allocator.
        pub type DefaultByteAllocator = alloc_engine::TlsfByteAllocator;
    }
}

/// Standalone slab byte allocator retained for API compatibility.
///
/// The slab build's [`GlobalAllocator`] does not use this wrapper: it owns
/// class-specific central caches directly.
#[cfg(feature = "slab")]
pub type DefaultByteAllocator = alloc_engine::SlabByteAllocator;

/// Kinds of memory usage for tracking.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, VariantArray, IntoStaticStr)]
pub enum UsageKind {
    /// Heap allocations made by kernel Rust code.
    RustHeap,
    /// Virtual memory, usually used for user space.
    VirtMem,
    /// Page cache for file systems.
    PageCache,
    /// Page tables.
    PageTable,
    /// DMA memory.
    Dma,
    /// Memory used by [`GlobalPage`].
    Global,
}

/// Statistics of memory usage by category.
#[derive(Clone, Copy)]
pub struct Usages([usize; UsageKind::VARIANTS.len()]);

impl Usages {
    /// Create a zero-initialized usage table.
    const fn new() -> Self {
        Self([0; UsageKind::VARIANTS.len()])
    }

    fn alloc(&mut self, kind: UsageKind, size: usize) {
        self.0[kind as usize] += size;
    }

    fn dealloc(&mut self, kind: UsageKind, size: usize) {
        self.0[kind as usize] = self.0[kind as usize]
            .checked_sub(size)
            .expect("kalloc usage underflow");
    }

    /// Return usage in bytes for the given kind.
    pub fn get(&self, kind: UsageKind) -> usize {
        self.0[kind as usize]
    }
}

impl fmt::Debug for Usages {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("UsageStats");
        for &kind in UsageKind::VARIANTS {
            d.field(kind.into(), &self.0[kind as usize]);
        }
        d.finish()
    }
}

/// Per-CPU atomic deltas backing the [`Usages`] diagnostic snapshot.
///
/// Allocation and deallocation can race a hardirq on the same CPU, and an
/// object may be released on a different CPU from the one that allocated it.
/// Each CPU therefore records signed local deltas rather than an ownership
/// balance. Their relaxed ordering is sufficient: users of
/// [`GlobalAllocator::usages`] need a diagnostic aggregate, not a globally
/// linearizable transaction history.
struct PerCpuUsages([AtomicIsize; UsageKind::VARIANTS.len()]);

impl PerCpuUsages {
    const fn new() -> Self {
        Self([const { AtomicIsize::new(0) }; UsageKind::VARIANTS.len()])
    }

    fn alloc(&self, kind: UsageKind, size: usize) {
        self.0[kind as usize].fetch_add(usage_delta(size), Ordering::Relaxed);
    }

    fn dealloc(&self, kind: UsageKind, size: usize) {
        self.0[kind as usize].fetch_sub(usage_delta(size), Ordering::Relaxed);
    }

    fn add_to(&self, totals: &mut [isize; UsageKind::VARIANTS.len()]) {
        for &kind in UsageKind::VARIANTS {
            let bytes = self.0[kind as usize].load(Ordering::Relaxed);
            totals[kind as usize] = totals[kind as usize].saturating_add(bytes);
        }
    }
}

#[inline]
fn usage_delta(size: usize) -> isize {
    isize::try_from(size).expect("kalloc usage value exceeds isize")
}

#[percpu::def_percpu]
static PER_CPU_USAGES: PerCpuUsages = PerCpuUsages::new();

#[inline]
fn add_usage(kind: UsageKind, size: usize) {
    PER_CPU_USAGES.with_current(|usages| usages.alloc(kind, size));
}

#[inline]
fn subtract_usage(kind: UsageKind, size: usize) {
    PER_CPU_USAGES.with_current(|usages| usages.dealloc(kind, size));
}

fn usage_snapshot() -> Usages {
    let mut totals = [0isize; UsageKind::VARIANTS.len()];
    for cpu in 0..kbuild_config::NR_CPUS {
        // SAFETY: `cpu` is bounded by the same compile-time limit used to
        // create the per-CPU allocator storage. The counters are atomic, so
        // concurrent task or hardirq updates are safe to observe.
        unsafe { PER_CPU_USAGES.remote_ref_raw(cpu) }.add_to(&mut totals);
    }

    let mut usages = Usages::new();
    for &kind in UsageKind::VARIANTS {
        // A concurrent cross-CPU allocation/free pair can be observed in
        // either order, so the aggregate is intentionally a best-effort
        // diagnostic snapshot rather than a linearizable balance.
        usages.0[kind as usize] = totals[kind as usize].max(0) as usize;
    }
    usages
}

/// The global allocator used by x-kernel.
///
/// In slab builds, small allocations are served by class-owned central slab
/// caches backed directly by the page allocator.  Other allocator builds keep
/// the existing byte-allocator-plus-page-allocator layering.
pub struct GlobalAllocator {
    #[cfg(feature = "slab")]
    slab_caches: KmemCacheSet,
    #[cfg(not(feature = "slab"))]
    balloc: SpinNoIrq<DefaultByteAllocator>,
    #[cfg(not(feature = "slab"))]
    /// Whether the byte allocator already owns a bootstrap heap region.
    heap_ready: AtomicBool,
    /// Whether the page allocator has at least one usable memory region.
    page_ready: AtomicBool,
    palloc: SpinNoIrq<BuddyPageAllocator<{ PAGE_SIZE_4K }>>,
    /// Self-contained accounting for explicitly constructed test allocators.
    ///
    /// The runtime singleton uses [`PER_CPU_USAGES`] instead so its hot path
    /// has no global statistics lock.
    usages: SpinNoIrq<Usages>,
}

/// Local sequence source for reclaim when no IPI provider is linked.
#[cfg(all(feature = "slab", not(feature = "ipi")))]
static NEXT_LOCAL_MAGAZINE_FLUSH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A reclaim session that keeps runtime task magazines inaccessible while
/// their objects are returned to central slab caches.
///
/// The initiator remains pinned for the session, but IRQs stay enabled.  This
/// makes a concurrent local magazine operation finish on the same CPU and
/// avoids a second task on the initiator CPU recursively waiting for the same
/// serialized global flush.
#[cfg(feature = "slab")]
struct MagazineReclaimGuard {
    sequence: u64,
    _no_preempt: Option<NoPreempt>,
}

#[cfg(feature = "slab")]
impl MagazineReclaimGuard {
    fn begin(allocator: &GlobalAllocator) -> Self {
        if !allocator.is_runtime_global_allocator() {
            return Self {
                sequence: 0,
                _no_preempt: None,
            };
        }

        let no_preempt = NoPreempt::new();
        #[cfg(feature = "ipi")]
        let sequence = KmemCacheFlushIf::begin_global_magazine_flush();
        #[cfg(not(feature = "ipi"))]
        let sequence = {
            let sequence = next_local_magazine_flush_sequence();
            if allocator
                .slab_caches
                .freeze_and_drain_current_task_magazines(sequence)
            {
                sequence
            } else {
                0
            }
        };

        if sequence == 0 {
            drop(no_preempt);
            return Self {
                sequence,
                _no_preempt: None,
            };
        }

        Self {
            sequence,
            _no_preempt: Some(no_preempt),
        }
    }
}

#[cfg(feature = "slab")]
impl Drop for MagazineReclaimGuard {
    fn drop(&mut self) {
        if self.sequence == 0 {
            return;
        }

        #[cfg(feature = "ipi")]
        KmemCacheFlushIf::end_global_magazine_flush(self.sequence);
        #[cfg(not(feature = "ipi"))]
        GLOBAL_ALLOCATOR
            .slab_caches
            .unfreeze_current_task_magazines(self.sequence);
    }
}

#[cfg(all(feature = "slab", not(feature = "ipi")))]
fn next_local_magazine_flush_sequence() -> u64 {
    loop {
        let current = NEXT_LOCAL_MAGAZINE_FLUSH_SEQUENCE.load(Ordering::Relaxed);
        let next = if current == TASK_MAGAZINE_FLUSH_SEQUENCE_MAX {
            1
        } else {
            current + 1
        };
        if NEXT_LOCAL_MAGAZINE_FLUSH_SEQUENCE
            .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

impl Default for GlobalAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalAllocator {
    /// Creates an empty [`GlobalAllocator`].
    pub const fn new() -> Self {
        Self {
            #[cfg(feature = "slab")]
            slab_caches: KmemCacheSet::new(),
            #[cfg(not(feature = "slab"))]
            balloc: SpinNoIrq::new(DefaultByteAllocator::new()),
            #[cfg(not(feature = "slab"))]
            heap_ready: AtomicBool::new(false),
            page_ready: AtomicBool::new(false),
            palloc: SpinNoIrq::new(BuddyPageAllocator::new()),
            usages: SpinNoIrq::new(Usages::new()),
        }
    }

    #[inline]
    fn is_runtime_global_allocator(&self) -> bool {
        core::ptr::eq(self, &GLOBAL_ALLOCATOR)
    }

    #[inline]
    fn uses_per_cpu_usage_stats(&self) -> bool {
        self.is_runtime_global_allocator()
    }

    #[inline]
    fn add_usage(&self, kind: UsageKind, size: usize) {
        if self.uses_per_cpu_usage_stats() {
            add_usage(kind, size);
        } else {
            self.usages.lock().alloc(kind, size);
        }
    }

    #[inline]
    fn subtract_usage(&self, kind: UsageKind, size: usize) {
        if self.uses_per_cpu_usage_stats() {
            subtract_usage(kind, size);
        } else {
            self.usages.lock().dealloc(kind, size);
        }
    }

    /// Returns the name of the allocator.
    pub const fn name(&self) -> &'static str {
        cfg_if::cfg_if! {
            if #[cfg(feature = "slab")] {
                "slab"
            } else if #[cfg(feature = "buddy")] {
                "buddy"
            } else if #[cfg(feature = "tlsf")] {
                "TLSF"
            }
        }
    }

    /// Initializes the allocator with the given region.
    ///
    /// Slab builds make the region immediately available to their backing page
    /// allocator.  Other builds also bootstrap their byte allocator.
    pub fn init(&self, va: usize, size: usize) {
        #[cfg(not(feature = "slab"))]
        assert!(size > MIN_HEAP_SIZE);
        self.init_or_extend_page_allocator(va, size)
            .expect("failed to initialize page allocator");
        #[cfg(not(feature = "slab"))]
        self.bootstrap_heap_if_needed()
            .expect("failed to initialize heap memory");
    }

    /// Initialize or extend only the allocator's backing page pool.
    ///
    /// Unlike [`init`](Self::init), this does not bootstrap a non-slab byte
    /// allocator. `va..va + size` must designate mapped, writable memory that
    /// has no other owner.
    ///
    /// # Panics
    ///
    /// Panics when the region is invalid for the backing page allocator.
    pub fn init_page_allocator(&self, va: usize, size: usize) {
        self.init_or_extend_page_allocator(va, size)
            .expect("failed to initialize page allocator");
    }

    /// Returns whether at least one backing page region has been registered.
    pub fn is_page_allocator_ready(&self) -> bool {
        self.page_ready.load(Ordering::Acquire)
    }

    /// Add the given region to the allocator.
    ///
    /// By default new memory first becomes available to the page allocator.
    pub fn add_memory(&self, va: usize, size: usize) -> AllocResult {
        self.init_or_extend_page_allocator(va, size)?;

        #[cfg(not(feature = "slab"))]
        if !self.heap_ready.load(Ordering::Acquire) {
            self.bootstrap_heap_if_needed()?;
        }
        Ok(())
    }

    /// Allocate arbitrary number of bytes. Returns the left bound of the
    /// allocated region.
    ///
    /// Slab builds route layouts represented by [`SizeClass`] to class-owned
    /// caches; other layouts are allocated directly from pages. Normal task
    /// and softirq paths use the current CPU magazine, while normal hardirq
    /// paths use a finite class reserve, with a one-object central-cache
    /// fallback when that reserve is empty; [`AllocError::NoMemory`] is
    /// returned only when both sources are empty. NMI allocation is
    /// unsupported. Other builds retain the byte-allocator expansion path.
    pub fn alloc(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        #[cfg(feature = "slab")]
        {
            self.alloc_slab(layout)
        }

        #[cfg(not(feature = "slab"))]
        {
            self.alloc_byte(layout)
        }
    }

    #[cfg(feature = "slab")]
    fn alloc_slab(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        let Some(size_class) = SizeClass::from_layout(layout) else {
            return self.large_alloc(layout);
        };

        let object = self.alloc_slab_class(size_class)?;
        self.add_usage(UsageKind::RustHeap, layout.size());
        Ok(object)
    }

    #[cfg(feature = "slab")]
    fn alloc_slab_class(&self, size_class: SizeClass) -> AllocResult<NonNull<u8>> {
        if !self.is_runtime_global_allocator() {
            return self.alloc_slab_class_centrally(size_class);
        }

        if kirq::context::is_in_hardirq_fast() {
            return self
                .slab_caches
                .try_allocate_irq_or_central(size_class)
                .ok_or(AllocError::NoMemory);
        }

        self.alloc_slab_class_from_magazine(size_class)
    }

    /// Allocate one class object through the runtime task magazine.
    ///
    /// The hardirq route is selected before entering this method. Its local
    /// accesses use only the `NoPreempt` guard supplied by `percpu`; refill and
    /// drain run with IRQs enabled between one-object central transfers.
    #[cfg(feature = "slab")]
    fn alloc_slab_class_from_magazine(&self, size_class: SizeClass) -> AllocResult<NonNull<u8>> {
        loop {
            // SAFETY: the caller has already excluded normal hardirq context.
            // NMI allocation is unsupported by the allocator contract, so no
            // interrupt-like path can concurrently mutate the task magazine.
            match unsafe { self.slab_caches.try_allocate_from_task_magazine(size_class) } {
                TaskMagazineAccess::Ready(Some(object)) => return Ok(object),
                TaskMagazineAccess::Frozen => {
                    // A reclaim session owns this CPU's magazine contents.
                    // Falling back centrally lets allocation make progress
                    // without waiting for the shrink initiator.
                    return self.alloc_slab_class_centrally(size_class);
                }
                TaskMagazineAccess::Ready(None) => {}
            }

            // A reserve refill is part of the local-miss slow path. Leaving an
            // under-full reserve visible to task frees lets hot magazine hits
            // stay local while still replenishing the reserve opportunistically.
            self.slab_caches.replenish_irq_reserve(size_class);
            let mut refill = ObjectBatch::new();
            self.slab_caches
                .take_task_batch(size_class, refill_count(size_class), &mut refill);
            if let Some(object) = refill.pop() {
                // SAFETY: `refill` contains only objects taken from this
                // matching class. The execution-context invariant above also
                // covers the short local install operation.
                let uninstalled =
                    match unsafe { self.slab_caches.install_task_refill(size_class, refill) } {
                        TaskMagazineRefill::Installed(batch)
                        | TaskMagazineRefill::Frozen(batch) => batch,
                    };
                // SAFETY: an uninstalled batch still contains only live
                // objects checked out from this matching central class.
                unsafe { self.slab_caches.return_task_batch(size_class, uninstalled) };
                return Ok(object);
            }

            self.populate_slab_class(size_class)?;
        }
    }

    /// Allocate one class object directly from central slabs.
    ///
    /// Explicitly constructed `GlobalAllocator` values use this path so their
    /// test-local state cannot alias the runtime singleton's per-CPU storage.
    #[cfg(feature = "slab")]
    fn alloc_slab_class_centrally(&self, size_class: SizeClass) -> AllocResult<NonNull<u8>> {
        loop {
            if let Some(object) = self.slab_caches.try_allocate(size_class) {
                return Ok(object);
            }
            self.populate_slab_class(size_class)?;
        }
    }

    /// Add a prepared span to one central slab cache.
    ///
    /// The retry after `NoMemory` first freezes and drains runtime task
    /// magazines, returns hardirq-reserve objects to central caches, and then
    /// gives every empty span back to the page allocator before retrying.
    #[cfg(feature = "slab")]
    fn populate_slab_class(&self, size_class: SizeClass) -> AllocResult {
        let page_count = KmemCacheSet::slab_pages(size_class);
        let span_alignment = KmemCacheSet::slab_bytes(size_class);
        let mut reclaimed_once = false;
        let base = loop {
            match self.alloc_pages(page_count, span_alignment, UsageKind::RustHeap) {
                Ok(base) => break base,
                Err(AllocError::NoMemory) if !reclaimed_once => {
                    reclaimed_once = true;
                    self.reclaim_empty_slab_pages();
                    if self.slab_caches.has_available_object(size_class) {
                        return Ok(());
                    }
                }
                Err(error) => return Err(error),
            }
        };
        // SAFETY: a successful page allocation is non-null, mapped,
        // exclusive, and satisfies the class span's requested alignment and
        // page count.
        let backing = unsafe { NonNull::new_unchecked(base as *mut u8) };
        // SAFETY: `backing` was just allocated with this class's exact span
        // geometry and remains exclusively owned until it is detached again.
        let prepared = match unsafe { KmemCacheSet::prepare_slab(size_class, backing) } {
            Ok(prepared) => prepared,
            Err(error) => {
                self.release_slab_pages(base, page_count);
                return Err(error);
            }
        };

        if let Err(prepared) = self.slab_caches.publish_slab(size_class, prepared) {
            let backing = prepared.into_backing();
            self.release_slab_backing(backing);
            return Err(AllocError::InvalidInput);
        }
        Ok(())
    }

    #[cfg(feature = "slab")]
    fn reclaim_empty_slab_pages(&self) -> usize {
        assert!(
            !kirq::context::is_in_hardirq_fast(),
            "slab reclaim must not run from hardirq context"
        );
        let _magazine_flush = MagazineReclaimGuard::begin(self);
        self.slab_caches.drain_irq_reserves();

        let mut released_pages = 0usize;
        while let Some(backing) = self.slab_caches.take_empty_slab() {
            released_pages = released_pages.saturating_add(backing.pages());
            self.release_slab_backing(backing);
        }
        released_pages
    }

    #[cfg(feature = "slab")]
    fn release_slab_backing(&self, backing: SlabBacking) {
        self.release_slab_pages(backing.base().as_ptr() as usize, backing.pages());
    }

    #[cfg(feature = "slab")]
    fn release_slab_pages(&self, base: usize, page_count: usize) {
        self.palloc.lock().deallocate_pages(base, page_count);
    }

    #[cfg(not(feature = "slab"))]
    fn alloc_byte(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        if layout.size() > PAGE_SIZE_4K / 2 || layout.align() > PAGE_SIZE_4K / 2 {
            return self.large_alloc(layout);
        }

        let mut balloc = self.balloc.lock();
        loop {
            let heap_ready = self.heap_ready.load(Ordering::Acquire);
            if heap_ready {
                match balloc.allocate(layout) {
                    Ok(object) => {
                        self.add_usage(UsageKind::RustHeap, layout.size());
                        return Ok(object);
                    }
                    Err(AllocError::NoMemory) => {
                        // Byte allocator exhausted; fall through to expansion.
                    }
                    Err(error) => return Err(error),
                }
            }

            let old_size = if heap_ready { balloc.total_bytes() } else { 0 };
            let exp_size = old_size
                .max(layout.size())
                .next_power_of_two()
                .max(PAGE_SIZE_4K);
            let mut request_size = exp_size;
            let min_size = if heap_ready {
                PAGE_SIZE_4K.max(layout.size())
            } else {
                MIN_HEAP_SIZE.max(layout.size())
            };

            loop {
                if !heap_ready {
                    request_size = (request_size + MIN_HEAP_SIZE - 1) & !(MIN_HEAP_SIZE - 1);
                }
                let heap_addr = match self.alloc_pages(
                    request_size / PAGE_SIZE_4K,
                    PAGE_SIZE_4K,
                    UsageKind::RustHeap,
                ) {
                    Ok(addr) => addr,
                    Err(error) => {
                        request_size /= 2;
                        if request_size < min_size {
                            return Err(error);
                        }
                        continue;
                    }
                };
                debug!(
                    "expand heap memory: [{:#x}, {:#x})",
                    heap_addr,
                    heap_addr + request_size
                );
                if heap_ready {
                    balloc.add_region(heap_addr, request_size)?;
                } else {
                    balloc.init_region(heap_addr, request_size);
                    self.heap_ready.store(true, Ordering::Release);
                }
                break;
            }
        }
    }

    /// Allocate directly from the page allocator for large requests.
    fn large_alloc(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        let pages = layout.size().div_ceil(PAGE_SIZE_4K);
        let align = layout.align().max(PAGE_SIZE_4K);
        let addr = self
            .alloc_pages(pages, align, UsageKind::RustHeap)
            .inspect_err(|e| {
                error!(
                    "large_alloc: failed to allocate {} pages ({} bytes): {:?}",
                    pages,
                    layout.size(),
                    e
                );
            })?;
        // alloc_pages skips usages for RustHeap; track it here.
        self.add_usage(UsageKind::RustHeap, pages_to_bytes(pages));
        // SAFETY: `addr` is non-null and properly aligned.
        Ok(unsafe { NonNull::new_unchecked(addr as *mut u8) })
    }

    /// Gives back an allocated region.
    ///
    /// The region should be allocated by [`alloc`], and `align_pow2` should be
    /// the same as the one used in [`alloc`]. Otherwise, the behavior is
    /// undefined.
    ///
    /// In slab builds, normal task/softirq frees return to the current CPU
    /// magazine or opportunistically replenish the hardirq reserve. Normal
    /// hardirq frees use that reserve directly; NMI deallocation is
    /// unsupported.
    ///
    /// [`alloc`]: GlobalAllocator::alloc
    pub fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        #[cfg(feature = "slab")]
        if let Some(size_class) = SizeClass::from_layout(layout) {
            self.subtract_usage(UsageKind::RustHeap, layout.size());
            self.dealloc_slab_class(size_class, ptr);
            return;
        }

        #[cfg(feature = "slab")]
        {
            let pages = layout.size().div_ceil(PAGE_SIZE_4K);
            self.dealloc_pages(ptr.as_ptr() as usize, pages, UsageKind::RustHeap);
        }

        #[cfg(not(feature = "slab"))]
        if layout.size() > PAGE_SIZE_4K / 2 || layout.align() > PAGE_SIZE_4K / 2 {
            let pages = layout.size().div_ceil(PAGE_SIZE_4K);
            self.dealloc_pages(ptr.as_ptr() as usize, pages, UsageKind::RustHeap);
        } else {
            self.subtract_usage(UsageKind::RustHeap, layout.size());
            self.balloc.lock().deallocate(ptr, layout);
        }
    }

    #[cfg(feature = "slab")]
    fn dealloc_slab_class(&self, size_class: SizeClass, object: NonNull<u8>) {
        if !self.is_runtime_global_allocator() {
            // SAFETY: `dealloc` already established that `object` is live and
            // the matching layout selected this size class.
            unsafe { self.slab_caches.deallocate(size_class, object) };
            return;
        }

        if kirq::context::is_in_hardirq_fast() {
            // SAFETY: hardirq callers still satisfy the `GlobalAlloc`
            // allocation/deallocation pairing contract. The reserve is the
            // only slab path allowed to mutate in this execution context.
            unsafe { self.slab_caches.deallocate_irq(size_class, object) };
            return;
        }

        if self.slab_caches.is_current_task_magazine_frozen() {
            // SAFETY: reclaim has made task magazines unavailable on this
            // CPU, so this ordinary task free returns directly to the
            // matching central cache instead of retaining an object locally.
            unsafe { self.slab_caches.deallocate(size_class, object) };
            return;
        }

        if self.slab_caches.irq_reserve_needs_refill(size_class) {
            // SAFETY: this is an ordinary task-context free of a live object
            // in the matching class. A reserve donation does not expose it to
            // a task magazine concurrently.
            if unsafe {
                self.slab_caches
                    .try_deallocate_to_irq_reserve(size_class, object)
            } {
                return;
            }
        }

        // SAFETY: normal hardirq execution was routed away above. NMI
        // allocation is unsupported, so this task/softirq operation cannot
        // race an interrupt-like magazine mutation on the same CPU.
        let drain = unsafe {
            self.slab_caches
                .deallocate_to_task_magazine(size_class, object)
        };
        match drain {
            TaskMagazineAccess::Ready(Some(batch)) => {
                // SAFETY: the local magazine extracted only live objects from
                // this matching size class.
                unsafe { self.slab_caches.return_task_batch(size_class, batch) };
            }
            TaskMagazineAccess::Ready(None) => {}
            TaskMagazineAccess::Frozen => {
                // SAFETY: the state machine left `object` out of the local
                // magazine, so it remains a live object of this matching
                // central cache.
                unsafe { self.slab_caches.deallocate(size_class, object) };
            }
        }
    }

    /// Allocates contiguous pages.
    ///
    /// It allocates `num_pages` pages from the page allocator.
    ///
    /// `align_pow2` must be a power of 2, and the returned region bound will be
    /// aligned to it.
    ///
    /// 1–4 page allocations with standard alignment are served from the
    /// per-CPU page cache to reduce global lock contention.
    pub fn alloc_pages(
        &self,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<usize> {
        debug_assert!(
            self.page_ready.load(Ordering::Acquire),
            "page allocator is not initialized"
        );

        // Fast path: 1–4 pages with standard alignment -> PCP cache.
        if (1..=pcp::PCP_MAX_PAGES).contains(&num_pages) && align_pow2 <= PAGE_SIZE_4K {
            let _guard = IrqSave::new();
            if let Some(addr) = pcp::try_alloc(num_pages) {
                drop(_guard);
                if !matches!(kind, UsageKind::RustHeap) {
                    self.add_usage(kind, pages_to_bytes(num_pages));
                }
                return Ok(addr);
            }
            drop(_guard);

            // Cache miss: batch-fill from the global allocator.
            let mut palloc = self.palloc.lock();
            let result = pcp::fill_cache(num_pages, &mut palloc);
            drop(palloc);
            if result.is_ok() && !matches!(kind, UsageKind::RustHeap) {
                self.add_usage(kind, pages_to_bytes(num_pages));
            }
            return result;
        }

        // Slow path: multi-page or special alignment.
        let addr = self.palloc.lock().allocate_pages(num_pages, align_pow2)?;
        if !matches!(kind, UsageKind::RustHeap) {
            self.add_usage(kind, pages_to_bytes(num_pages));
        }
        Ok(addr)
    }

    /// Allocates contiguous DMA pages.
    ///
    /// DMA allocations must come from physical memory below 4 GiB.
    /// The page allocator only knows virtual addresses, so the
    /// DMA32 filtering happens here using `v2p` translation.
    pub fn alloc_dma_pages(
        &self,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<usize> {
        debug_assert!(
            self.page_ready.load(Ordering::Acquire),
            "page allocator is not initialized"
        );
        const DMA32_LIMIT: usize = 0x1_0000_0000;

        let addr = self.palloc.lock().allocate_pages(num_pages, align_pow2)?;

        let phys = kernel_virt_to_phys(addr);
        let size = pages_to_bytes(num_pages);
        if phys + size > DMA32_LIMIT {
            self.palloc.lock().deallocate_pages(addr, num_pages);
            return Err(AllocError::NoMemory);
        }

        if !matches!(kind, UsageKind::RustHeap) {
            self.add_usage(kind, size);
        }
        Ok(addr)
    }

    /// Allocates contiguous pages starting from the given address.
    ///
    /// It allocates `num_pages` pages from the page allocator starting from the
    /// given address.
    ///
    /// `align_pow2` must be a power of 2, and the returned region bound will be
    /// aligned to it.
    pub fn alloc_pages_at(
        &self,
        va: usize,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<usize> {
        let addr = self
            .palloc
            .lock()
            .allocate_pages_at(va, num_pages, align_pow2)?;
        if kind != UsageKind::RustHeap {
            self.add_usage(kind, pages_to_bytes(num_pages));
        }
        Ok(addr)
    }

    /// Gives back the allocated pages starts from `va` to the page allocator.
    ///
    /// The pages should be allocated by [`alloc_pages`], and `align_pow2`
    /// should be the same as the one used in [`alloc_pages`]. Otherwise, the
    /// behavior is undefined.
    ///
    /// 1–4 page deallocations are cached per-CPU to reduce global lock
    /// contention.
    ///
    /// [`alloc_pages`]: GlobalAllocator::alloc_pages
    pub fn dealloc_pages(&self, va: usize, num_pages: usize, kind: UsageKind) {
        self.subtract_usage(kind, pages_to_bytes(num_pages));

        // Fast path: 1–4 pages -> try to cache them per-CPU.
        if (1..=pcp::PCP_MAX_PAGES).contains(&num_pages) {
            let _guard = IrqSave::new();
            if pcp::try_free(num_pages, va) {
                return;
            }
            drop(_guard);

            // Cache full: bulk-drain to the global allocator.
            let mut palloc = self.palloc.lock();
            pcp::drain_cache(num_pages, &mut palloc, va);
            return;
        }

        // Slow path: multi-page.
        self.palloc.lock().deallocate_pages(va, num_pages);
    }

    /// Gives back the allocated DMA pages starts from `va` to the DMA page allocator.
    pub fn dealloc_dma_pages(&self, va: usize, num_pages: usize, kind: UsageKind) {
        self.subtract_usage(kind, pages_to_bytes(num_pages));
        self.palloc.lock().deallocate_pages(va, num_pages);
    }

    fn init_or_extend_page_allocator(&self, va: usize, size: usize) -> AllocResult {
        if self.page_ready.load(Ordering::Acquire) {
            self.palloc.lock().add_region(va, size)?;
        } else {
            self.palloc.lock().init_region(va, size);
            self.page_ready.store(true, Ordering::Release);
        }
        Ok(())
    }

    #[cfg(not(feature = "slab"))]
    fn bootstrap_heap_if_needed(&self) -> AllocResult {
        if self.heap_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let heap_addr = self.alloc_pages(
            MIN_HEAP_SIZE / PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            UsageKind::RustHeap,
        )?;
        self.balloc.lock().init_region(heap_addr, MIN_HEAP_SIZE);
        self.heap_ready.store(true, Ordering::Release);
        Ok(())
    }

    /// Returns bytes currently checked out from the small-object allocator.
    ///
    /// In slab builds this includes objects retained in task-local magazines,
    /// but excludes hardirq reserve objects because they are allocatable by
    /// that execution context.
    pub fn used_bytes(&self) -> usize {
        #[cfg(feature = "slab")]
        {
            self.slab_caches.used_bytes()
        }

        #[cfg(not(feature = "slab"))]
        {
            self.balloc.lock().used_bytes()
        }
    }

    /// Returns centrally available small-object bytes.
    ///
    /// In slab builds this includes the hardirq reserve but excludes objects
    /// held in task-local magazines, for which this diagnostic has no
    /// synchronized cross-CPU view.
    pub fn available_bytes(&self) -> usize {
        #[cfg(feature = "slab")]
        {
            self.slab_caches.available_bytes()
        }

        #[cfg(not(feature = "slab"))]
        {
            self.balloc.lock().available_bytes()
        }
    }

    /// Reclaim all completely empty central slab spans.
    ///
    /// In slab builds this first flushes runtime task magazines, drains
    /// hardirq reserves, and then returns detached empty spans to the page
    /// allocator. It must be called outside hardirq and NMI context; a slab
    /// hardirq invocation panics rather than waiting for cross-CPU reclaim.
    ///
    /// Returns the number of base pages returned to the page allocator. In
    /// non-slab builds no central slab spans exist, so this returns zero.
    pub fn shrink(&self) -> usize {
        #[cfg(feature = "slab")]
        {
            self.reclaim_empty_slab_pages()
        }

        #[cfg(not(feature = "slab"))]
        {
            0
        }
    }

    /// Returns the number of allocated pages in the page allocator.
    pub fn used_pages(&self) -> usize {
        self.palloc.lock().used_pages()
    }

    /// Returns the number of available pages in the page allocator.
    pub fn available_pages(&self) -> usize {
        self.palloc.lock().available_pages()
    }

    /// Returns the usage statistics of the allocator.
    ///
    /// The runtime global allocator returns a best-effort aggregate over its
    /// per-CPU update deltas. Explicitly constructed allocator instances keep
    /// a self-contained, locked usage table for test isolation.
    pub fn usages(&self) -> Usages {
        if self.uses_per_cpu_usage_stats() {
            usage_snapshot()
        } else {
            *self.usages.lock()
        }
    }
}

// SAFETY: `GlobalAllocator` routes all allocations through its internal locked
// allocators and upholds the `GlobalAlloc` contract for returned pointers.
unsafe impl GlobalAlloc for GlobalAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let allocate_memory = || {
            if let Ok(ptr) = GlobalAllocator::alloc(self, layout) {
                ptr.as_ptr()
            } else {
                let used = self.used_bytes();
                let avail = self.available_bytes();
                error!(
                    "kalloc OOM: layout {:?} (size={}, align={}), heap used={:#x} avail={:#x}",
                    layout,
                    layout.size(),
                    layout.align(),
                    used,
                    avail
                );
                alloc::alloc::handle_alloc_error(layout)
            }
        };

        #[cfg(feature = "tracking")]
        {
            track_allocation(allocate_memory, layout)
        }

        #[cfg(not(feature = "tracking"))]
        {
            allocate_memory()
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let ptr = NonNull::new(ptr).expect("dealloc null ptr");

        let deallocate_memory = || {
            GlobalAllocator::dealloc(self, ptr, layout);
        };

        #[cfg(feature = "tracking")]
        {
            track_deallocation(ptr, deallocate_memory);
        }

        #[cfg(not(feature = "tracking"))]
        {
            deallocate_memory();
        }
    }
}

#[cfg(feature = "tracking")]
fn track_allocation<F>(allocate_memory: F, layout: Layout) -> *mut u8
where
    F: FnOnce() -> *mut u8,
{
    tracking::with_state(|state| match state {
        None => allocate_memory(),
        Some(state) => {
            let ptr = allocate_memory();
            let generation = state.generation;
            state.generation += 1;
            state.map.insert(
                ptr as usize,
                tracking::AllocationInfo {
                    layout,
                    backtrace: backtrace::Backtrace::capture(),
                    generation,
                },
            );
            ptr
        }
    })
}

#[cfg(feature = "tracking")]
fn track_deallocation<F>(ptr: NonNull<u8>, deallocate_memory: F)
where
    F: FnOnce(),
{
    tracking::with_state(|state| match state {
        None => deallocate_memory(),
        Some(state) => {
            let address = ptr.as_ptr() as usize;
            state.map.remove(&address);
            deallocate_memory();
        }
    });
}

#[cfg_attr(all(target_os = "none", not(test)), global_allocator)]
static GLOBAL_ALLOCATOR: GlobalAllocator = GlobalAllocator::new();

/// Returns the reference to the global allocator.
pub fn global_allocator() -> &'static GlobalAllocator {
    &GLOBAL_ALLOCATOR
}

/// Stage a global-reclaim freeze request for one present logical CPU.
///
/// This is the `kipi` integration hook.  It only changes atomic access state;
/// the matching IPI handler must call
/// [`complete_current_task_magazine_flush`] on the destination CPU before the
/// initiator drains that CPU's magazine storage.
#[cfg(all(feature = "slab", feature = "ipi"))]
pub fn request_task_magazine_flush_on_cpu(cpu: usize, sequence: u64) {
    GLOBAL_ALLOCATOR
        .slab_caches
        .request_task_magazine_flush_on_cpu(cpu, sequence);
}

/// Advance the current CPU's staged task-magazine request from its IPI handler.
///
/// Returns `true` once the CPU is frozen for `sequence`.  `false` means an
/// interrupted task still owns a local magazine borrow, or this was a delayed
/// IPI for a finished sequence; the initiator must rely on the completion
/// predicate rather than treating this result as an error.
#[cfg(all(feature = "slab", feature = "ipi"))]
pub fn complete_current_task_magazine_flush(sequence: u64) -> bool {
    GLOBAL_ALLOCATOR
        .slab_caches
        .complete_current_task_magazine_flush(sequence)
}

/// Return whether `cpu` has safely frozen its task magazines for `sequence`.
#[cfg(all(feature = "slab", feature = "ipi"))]
pub fn is_task_magazine_flush_complete(cpu: usize, sequence: u64) -> bool {
    GLOBAL_ALLOCATOR
        .slab_caches
        .is_task_magazine_flush_complete(cpu, sequence)
}

/// Drain all magazines on a CPU already frozen for `sequence`.
///
/// The caller must keep the corresponding global flush session active until
/// it later calls [`unfreeze_task_magazines_on_cpu`].  This function performs
/// no page allocation and no page release.
#[cfg(all(feature = "slab", feature = "ipi"))]
pub fn drain_frozen_task_magazines_on_cpu(cpu: usize, sequence: u64) -> bool {
    GLOBAL_ALLOCATOR
        .slab_caches
        .drain_frozen_task_magazines_on_cpu(cpu, sequence)
}

/// Release one CPU from a completed global task-magazine flush.
#[cfg(all(feature = "slab", feature = "ipi"))]
pub fn unfreeze_task_magazines_on_cpu(cpu: usize, sequence: u64) {
    GLOBAL_ALLOCATOR
        .slab_caches
        .unfreeze_task_magazines_on_cpu(cpu, sequence);
}

/// Initializes the global allocator with the given memory region.
///
/// Note that the memory region bounds are just numbers, and the allocator
/// does not actually access the region. Users should ensure that the region
/// is valid and not being used by others, so that the allocated memory is also
/// valid.
///
/// The first call bootstraps the allocator, and later calls extend it.
pub fn global_init(va: usize, size: usize) {
    debug!(
        "initialize global allocator at: [{:#x}, {:#x})",
        va,
        va + size
    );
    GLOBAL_ALLOCATOR.init(va, size);
}

/// Add the given memory region to the global allocator.
///
/// Users should ensure that the region is valid and not being used by others,
/// so that the allocated memory is also valid.
///
/// It's similar to [`global_init`], but can be called multiple times.
pub fn global_add_memory(va: usize, size: usize) -> AllocResult {
    debug!(
        "add a memory region to global allocator: [{:#x}, {:#x})",
        va,
        va + size
    );
    GLOBAL_ALLOCATOR.add_memory(va, size)
}

/// Returns whether the global allocator has a registered backing page region.
pub fn is_page_allocator_ready() -> bool {
    GLOBAL_ALLOCATOR.is_page_allocator_ready()
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_kalloc {
    #[cfg(feature = "slab")]
    use alloc::boxed::Box;
    #[cfg(feature = "slab")]
    use core::alloc::Layout;

    use strum::VariantArray;
    use unittest::def_test;

    #[cfg(feature = "slab")]
    use super::{GlobalAllocator, PAGE_SIZE_4K};
    use super::{PerCpuUsages, UsageKind, Usages};

    #[cfg(feature = "slab")]
    const TEST_HEAP_PAGES: usize = 4;

    #[cfg(feature = "slab")]
    #[repr(align(16384))]
    struct AlignedTestHeap([u8; TEST_HEAP_PAGES * PAGE_SIZE_4K]);

    #[def_test]
    fn test_usages_alloc_dealloc() {
        let mut usages = Usages::new();
        usages.alloc(UsageKind::RustHeap, 64);
        assert_eq!(usages.get(UsageKind::RustHeap), 64);
        usages.dealloc(UsageKind::RustHeap, 32);
        assert_eq!(usages.get(UsageKind::RustHeap), 32);
    }

    #[def_test]
    fn test_usage_kind_variants_len() {
        assert!(UsageKind::VARIANTS.len() >= 5);
    }

    #[def_test]
    fn test_usages_independent_kinds() {
        let mut usages = Usages::new();
        usages.alloc(UsageKind::VirtMem, 10);
        usages.alloc(UsageKind::PageTable, 20);
        assert_eq!(usages.get(UsageKind::VirtMem), 10);
        assert_eq!(usages.get(UsageKind::PageTable), 20);
    }

    #[def_test]
    fn test_per_cpu_usage_snapshot() {
        let counters = PerCpuUsages::new();
        counters.alloc(UsageKind::RustHeap, 64);
        counters.alloc(UsageKind::PageCache, 128);
        counters.dealloc(UsageKind::RustHeap, 16);

        let mut totals = [0isize; UsageKind::VARIANTS.len()];
        counters.add_to(&mut totals);
        let mut usages = Usages::new();
        for &kind in UsageKind::VARIANTS {
            usages.0[kind as usize] = totals[kind as usize].max(0) as usize;
        }
        assert_eq!(usages.get(UsageKind::RustHeap), 48);
        assert_eq!(usages.get(UsageKind::PageCache), 128);
    }

    #[def_test]
    fn test_constructed_allocator_keeps_own_usage_snapshot() {
        let allocator = GlobalAllocator::new();
        allocator.add_usage(UsageKind::RustHeap, 64);
        assert_eq!(allocator.usages().get(UsageKind::RustHeap), 64);
        allocator.subtract_usage(UsageKind::RustHeap, 64);
        assert_eq!(allocator.usages().get(UsageKind::RustHeap), 0);
    }

    #[cfg(feature = "slab")]
    #[def_test]
    fn test_slab_oom_reclaims_empty_spans_before_retrying() {
        let heap = Box::new(AlignedTestHeap([0; TEST_HEAP_PAGES * PAGE_SIZE_4K]));
        let allocator = GlobalAllocator::new();
        allocator.init_page_allocator(heap.0.as_ptr() as usize, heap.0.len());

        let layout_2048 = Layout::from_size_align(2048, 2048).unwrap();
        let first = allocator.alloc(layout_2048).unwrap();
        allocator.dealloc(first, layout_2048);

        // The 2 KiB class owns the only four pages as an empty span.  The
        // 1 KiB class must reclaim that span before its two-page retry can
        // succeed.
        let layout_1024 = Layout::from_size_align(1024, 1024).unwrap();
        let second = allocator.alloc(layout_1024).unwrap();
        assert_eq!(allocator.used_bytes(), 1024);
        allocator.dealloc(second, layout_1024);
        assert_eq!(allocator.shrink(), 2);
        assert_eq!(allocator.used_bytes(), 0);
    }
}
