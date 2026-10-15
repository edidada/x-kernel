// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel global allocator and page allocation helpers.
#![allow(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): legacy kalloc has 15 diagnostic \
              sites, too many to judge individually in this migration; new code remains reviewed"
)]
#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

use core::{
    alloc::{GlobalAlloc, Layout},
    fmt,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use alloc_engine::{
    AllocError, AllocResult, BaseAllocator, BuddyPageAllocator, ByteAllocator, PageAllocator,
};
use kcpu_slot::{CpuId, CpuPin, cpu_slot_cell};
use klazy::Once;
use kspin::{NoPreempt, NoPreemptIrqSave, SpinNoIrq};
use memaddr::PAGE_SIZE_4K;
use strum::{IntoStaticStr, VariantArray};
const MIN_HEAP_SIZE: usize = 0x8000; // 32 K

/// Keep medium-sized buffers in the byte heap so they can reuse memory already
/// transferred from the page allocator instead of requiring a new high-order block.
const LARGE_ALLOC_THRESHOLD_BYTES: usize = 1024 * 1024;

fn pages_to_bytes(num_pages: usize) -> usize {
    num_pages
        .checked_mul(PAGE_SIZE_4K)
        .expect("page count byte size overflow")
}

mod buddy_frame_adapter;
mod frame;
mod frame_bootstrap;
mod frame_mapping;
mod page;
mod pcp;
#[cfg(feature = "slab")]
mod slab_cache;
pub use frame::{FrameMeta, FrameRef, FrameRun, FrameState};
pub use frame_bootstrap::{FrameBootstrapInfo, FrameRegion};
pub use page::GlobalPage;

#[cfg(feature = "tracking")]
mod tracking;
#[cfg(feature = "tracking")]
pub use tracking::*;

/// Exclusive access capability for the current CPU's allocator caches.
///
/// This type is crate-private so cache access cannot escape the allocator
/// entry points. Keeping its IRQ guard alive pins execution to one CPU and
/// excludes interrupt re-entry until the cache operation has completed.
pub(crate) struct AllocLocalOwner {
    guard: NoPreemptIrqSave,
}

impl AllocLocalOwner {
    #[inline]
    fn acquire() -> Self {
        Self {
            guard: NoPreemptIrqSave::new(),
        }
    }

    /// Returns the guard so cache accessors can borrow it mutably, which
    /// bounds the returned cache reference to the call.
    #[inline]
    fn guard(&mut self) -> &mut NoPreemptIrqSave {
        &mut self.guard
    }
}

// Exactly one allocator feature must be selected. Kernel builds get it
// through kfeat; host test builds pass it explicitly (e.g.
// `cargo test -p kext4 --features kalloc/slab`).
#[cfg(not(any(feature = "slab", feature = "buddy", feature = "tlsf")))]
compile_error!("kalloc requires one of the allocator features: slab, buddy, or tlsf");

cfg_if::cfg_if! {
    if #[cfg(feature = "slab")] {
        /// The default byte allocator.
        pub type DefaultByteAllocator = alloc_engine::SlabByteAllocator;
    } else if #[cfg(feature = "buddy")] {
        /// The default byte allocator.
        pub type DefaultByteAllocator = alloc_engine::BuddyByteAllocator;
    } else if #[cfg(feature = "tlsf")] {
        /// The default byte allocator.
        pub type DefaultByteAllocator = alloc_engine::TlsfByteAllocator;
    }
}

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
    /// Creates a zero-initialized usage snapshot.
    const fn new() -> Self {
        Self([0; UsageKind::VARIANTS.len()])
    }

    /// Return usage in bytes for the given kind.
    pub fn get(&self, kind: UsageKind) -> usize {
        self.0[kind as usize]
    }
}

/// Per-CPU cumulative counters from which [`Usages`] snapshots are collected.
struct UsageCounters {
    allocated_bytes: [AtomicUsize; UsageKind::VARIANTS.len()],
    freed_bytes: [AtomicUsize; UsageKind::VARIANTS.len()],
}

impl UsageCounters {
    const fn new() -> Self {
        Self {
            allocated_bytes: [const { AtomicUsize::new(0) }; UsageKind::VARIANTS.len()],
            freed_bytes: [const { AtomicUsize::new(0) }; UsageKind::VARIANTS.len()],
        }
    }

    fn alloc(&self, kind: UsageKind, size_bytes: usize) {
        self.allocated_bytes[kind as usize]
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(size_bytes)
            })
            .expect("kalloc usage overflow");
    }

    fn dealloc(&self, kind: UsageKind, size_bytes: usize) {
        // Release publishes the allocation that must precede every valid
        // free. The snapshot acquires freed totals before reading allocations.
        self.freed_bytes[kind as usize]
            .fetch_update(Ordering::Release, Ordering::Relaxed, |current| {
                current.checked_add(size_bytes)
            })
            .expect("kalloc freed usage overflow");
    }
}

struct UsageTotals {
    allocated_bytes: [u128; UsageKind::VARIANTS.len()],
    freed_bytes: [u128; UsageKind::VARIANTS.len()],
}

impl UsageTotals {
    const fn new() -> Self {
        Self {
            allocated_bytes: [0; UsageKind::VARIANTS.len()],
            freed_bytes: [0; UsageKind::VARIANTS.len()],
        }
    }

    fn collect_freed(&mut self, counters: &UsageCounters) {
        for &kind in UsageKind::VARIANTS {
            let index = kind as usize;
            self.freed_bytes[index] = self.freed_bytes[index]
                .checked_add(counters.freed_bytes[index].load(Ordering::Acquire) as u128)
                .expect("kalloc freed usage total overflow");
        }
    }

    fn collect_allocated(&mut self, counters: &UsageCounters) {
        for &kind in UsageKind::VARIANTS {
            let index = kind as usize;
            self.allocated_bytes[index] = self.allocated_bytes[index]
                .checked_add(counters.allocated_bytes[index].load(Ordering::Relaxed) as u128)
                .expect("kalloc allocated usage total overflow");
        }
    }

    fn finish(self) -> Usages {
        let mut usages = Usages::new();
        for &kind in UsageKind::VARIANTS {
            let index = kind as usize;
            let live_bytes = self.allocated_bytes[index]
                .checked_sub(self.freed_bytes[index])
                .expect("kalloc usage underflow");
            usages.0[index] = usize::try_from(live_bytes).expect("kalloc usage overflow");
        }
        usages
    }
}

// SAFETY: every field is an array of zero-initialized atomics with no
// ownership or drop state.
unsafe impl kcpu_slot::StaticSlotValue for crate::UsageCounters {}

cpu_slot_cell! {
    static PERCPU_USAGE_COUNTERS: crate::UsageCounters = crate::UsageCounters::new();
}

fn with_current_usage_counters<T>(f: impl FnOnce(&UsageCounters) -> T) -> T {
    let preempt_guard = NoPreempt::new();
    // Early-boot allocation can precede this CPU's area publication; skip the
    // diagnostic accounting instead of panicking on an unpublished area.
    let Some(area) = kcpu_slot::area(preempt_guard.current_cpu()) else {
        return f(&UsageCounters::new());
    };
    let counters = PERCPU_USAGE_COUNTERS.shared_at(area);
    f(counters)
}

fn usage_snapshot() -> Usages {
    let mut totals = UsageTotals::new();
    // `NR_CPUS` is the exact bound on publishable areas: `kcpu_slot::area`
    // indexes the `NR_CPUS`-sized area-state table, so every CPU that can ever
    // be published lies in this range. Areas are published monotonically and
    // never unpublished, so a CPU skipped here as unpublished can only be
    // skipped in the freed pass and then included in the allocated pass, which
    // keeps `allocated >= freed` and cannot trip the `finish` underflow check.
    let cpu_count = kbuild_config::NR_CPUS;

    // Read every freed counter first. Acquiring a recorded free also observes
    // its causally preceding allocation before the second pass, which prevents
    // a valid cross-CPU free from producing a spurious aggregate underflow.
    for cpu_index in 0..cpu_count {
        let Some(area) = kcpu_slot::area(CpuId::new(cpu_index)) else {
            continue;
        };
        let counters = PERCPU_USAGE_COUNTERS.shared_at(area);
        totals.collect_freed(counters);
    }
    for cpu_index in 0..cpu_count {
        let Some(area) = kcpu_slot::area(CpuId::new(cpu_index)) else {
            continue;
        };
        let counters = PERCPU_USAGE_COUNTERS.shared_at(area);
        totals.collect_allocated(counters);
    }

    totals.finish()
}

/// Reads cumulative successful heap allocation bytes on the guarded CPU for tests.
///
/// Keep the same IRQ/preemption guard across both readings and the measured
/// operation. Frees never reduce this counter; remote CPU allocations are excluded.
/// This is not live usage, an allocation-attempt counter or NMI attribution.
///
/// # Panics
/// Panics if this CPU's allocator accounting area has not been published.
#[cfg(unittest)]
pub fn current_cpu_allocated_bytes_for_test(guard: &NoPreemptIrqSave) -> usize {
    let area = kcpu_slot::area(guard.current_cpu()).expect("allocator CPU area is initialized");
    PERCPU_USAGE_COUNTERS.shared_at(area).allocated_bytes[UsageKind::RustHeap as usize]
        .load(Ordering::Relaxed)
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

/// The global allocator used by x-kernel.
///
/// It combines a [`ByteAllocator`] and a [`PageAllocator`] into a simple
/// two-level allocator: firstly tries allocate from the byte allocator, if
/// there is no memory, asks the page allocator for more memory and adds it to
/// the byte allocator.
///
/// The configured byte allocator serves sub-page requests, while
/// [`BuddyPageAllocator`] provides backing pages and direct large allocations.
/// Slab builds add a per-CPU object cache in front of the byte allocator.
pub struct GlobalAllocator {
    balloc: SpinNoIrq<DefaultByteAllocator>,
    /// Whether the byte allocator already owns a bootstrap heap region.
    heap_ready: AtomicBool,
    /// Whether the page allocator has at least one usable memory region.
    page_ready: AtomicBool,
    palloc: SpinNoIrq<BuddyPageAllocator<{ PAGE_SIZE_4K }>>,
    frames: Once<frame::FrameRegistry>,
    frame_mapping: Once<frame_mapping::LinearFrameMapping>,
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
            balloc: SpinNoIrq::new(DefaultByteAllocator::new()),
            heap_ready: AtomicBool::new(false),
            page_ready: AtomicBool::new(false),
            palloc: SpinNoIrq::new(BuddyPageAllocator::new()),
            frames: Once::new(),
            frame_mapping: Once::new(),
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
    /// This is the legacy "initialize both page allocator and byte allocator"
    /// entry used by paths that already have a stable runtime memory region.
    pub fn init(&self, va: usize, size: usize) {
        assert!(size > MIN_HEAP_SIZE);
        self.init_or_extend_page_allocator(va, size)
            .expect("failed to initialize page allocator");
        self.bootstrap_heap_if_needed()
            .expect("failed to initialize heap memory");
    }

    /// Initialize or extend only the backing page allocator.
    ///
    /// The byte allocator is bootstrapped lazily when memory is later added or
    /// a byte allocation needs it. `va..va + size` must be a mapped, writable,
    /// exclusive region that does not overlap an existing allocator region.
    pub fn init_page_allocator(&self, va: usize, size: usize) {
        self.init_or_extend_page_allocator(va, size)
            .expect("failed to initialize page allocator");
    }

    /// Initializes resident descriptors and the complete boot page inventory.
    ///
    /// Descriptor backing and buddy bitmaps are permanently excluded from
    /// allocations. This entry does not initialize the byte heap. The current
    /// VA-based buddy handoff requires one linear VA-to-PA offset; physical
    /// descriptor coverage is independent of it. Partial edge pages are excluded.
    /// Subsequent calls to the legacy region-extension APIs are rejected.
    ///
    /// # Errors
    ///
    /// Rejects an initialized allocator, overlapping or invalid ranges, too
    /// many regions, and the absence of contiguous descriptor backing. Failed
    /// planning does not write into RAM or publish allocator state.
    ///
    /// # Safety
    ///
    /// Every input range must be exclusively owned writable RAM mapped at its
    /// given VA, with both mapping and storage valid until this allocator and
    /// all of its users are destroyed. Initialization must be serialized with
    /// boot memory handoff; callers must not access the handed-off storage.
    pub unsafe fn init_page_allocator_with_regions(
        &self,
        regions: &[FrameRegion],
    ) -> AllocResult<FrameBootstrapInfo> {
        let mut pages = self.palloc.lock();
        if self.page_ready.load(Ordering::Acquire)
            || self.frames.get().is_some()
            || self.frame_mapping.get().is_some()
        {
            return Err(AllocError::AlreadyInitialized);
        }
        // SAFETY: the caller supplies exclusive, valid mapped RAM and keeps
        // it alive. The page lock serializes initialization, and readiness has
        // not yet been published to any allocation path.
        let (registry, mapping, info) = unsafe { frame_bootstrap::bootstrap(regions, &mut pages) }?;
        self.frame_mapping.call_once(|| mapping);
        self.frames.call_once(|| registry);
        self.page_ready.store(true, Ordering::Release);
        Ok(info)
    }

    /// Locates the resident descriptor for a covered physical base-page number.
    ///
    /// Missing means the PFN is outside this registry, not necessarily outside
    /// RAM. The returned borrow neither owns the page nor authorizes access to
    /// its current allocation or contents.
    pub fn frame_meta(&self, pfn: usize) -> Option<&FrameMeta> {
        self.frames.get()?.lookup(pfn)
    }

    /// Return whether the backing page allocator has a usable memory region.
    pub fn is_page_allocator_ready(&self) -> bool {
        self.page_ready.load(Ordering::Acquire)
    }

    /// Add the given region to the allocator.
    ///
    /// By default new memory first becomes available to the page allocator.
    pub fn add_memory(&self, va: usize, size: usize) -> AllocResult {
        self.init_or_extend_page_allocator(va, size)?;

        if !self.heap_ready.load(Ordering::Acquire) {
            self.bootstrap_heap_if_needed()?;
        }
        Ok(())
    }

    /// Allocate arbitrary number of bytes. Returns the left bound of the
    /// allocated region.
    ///
    /// It firstly tries to allocate from the byte allocator. If there is no
    /// memory, it asks the page allocator for more memory and adds it to the
    /// byte allocator.
    ///
    /// Large allocations (over `LARGE_ALLOC_THRESHOLD_BYTES`) bypass the byte allocator
    /// to avoid fragmenting the slab heap and are served directly from the
    /// page allocator.
    pub fn alloc(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        // Large allocations: go directly to the page allocator.
        if layout.size() > LARGE_ALLOC_THRESHOLD_BYTES {
            return self.large_alloc(layout);
        }

        #[cfg(feature = "slab")]
        if let Some(size_class) = alloc_engine::SizeClass::from_layout(layout) {
            let object = self.alloc_from_percpu_slab(size_class)?;
            self.record_usage_alloc(UsageKind::RustHeap, layout.size());
            return Ok(object);
        }

        let object = self.alloc_from_byte_allocator(layout)?;
        self.record_usage_alloc(UsageKind::RustHeap, layout.size());
        Ok(object)
    }

    /// Allocate through the globally locked byte allocator.
    fn alloc_from_byte_allocator(&self, layout: Layout) -> AllocResult<NonNull<u8>> {
        let mut balloc = self.balloc.lock();
        loop {
            let heap_ready = self.heap_ready.load(Ordering::Acquire);
            if heap_ready && let Ok(ptr) = balloc.allocate(layout) {
                return Ok(ptr);
            }

            self.grow_byte_allocator(&mut balloc, layout)?;
        }
    }

    /// Allocate a canonical slab object through the current CPU cache.
    #[cfg(feature = "slab")]
    fn alloc_from_percpu_slab(
        &self,
        size_class: alloc_engine::SizeClass,
    ) -> AllocResult<NonNull<u8>> {
        let mut local_owner = AllocLocalOwner::acquire();
        // `local_owner` excludes same-CPU interrupt re-entry and pins this
        // operation until the cache access has completed.
        if let Some(object) = slab_cache::try_alloc(&mut local_owner, size_class) {
            return Ok(object);
        }

        let class_size = size_class.size();
        let layout = Layout::from_size_align(class_size, class_size)
            .expect("slab size classes must be powers of two");
        let mut balloc = self.balloc.lock();
        loop {
            if self.heap_ready.load(Ordering::Acquire) {
                // SAFETY: `balloc` is the exclusively locked central slab
                // allocator, `local_owner` protects this CPU's cache, and no
                // object provenance is consumed by refill.
                let refilled =
                    unsafe { slab_cache::refill_cache(&mut local_owner, size_class, &mut *balloc) };
                if let Ok(object) = refilled {
                    return Ok(object);
                }
            }

            self.grow_byte_allocator(&mut balloc, layout)?;
        }
    }

    /// Add one region to the byte allocator, reducing the request under
    /// pressure until the minimum useful region size is reached.
    fn grow_byte_allocator(
        &self,
        balloc: &mut DefaultByteAllocator,
        layout: Layout,
    ) -> AllocResult {
        let heap_ready = self.heap_ready.load(Ordering::Acquire);
        let old_size = if heap_ready { balloc.total_bytes() } else { 0 };
        let exp_size = old_size
            .max(layout.size())
            .next_power_of_two()
            .max(PAGE_SIZE_4K);

        let mut req_size = exp_size;
        let min_size = if heap_ready {
            PAGE_SIZE_4K.max(layout.size())
        } else {
            MIN_HEAP_SIZE.max(layout.size())
        };
        loop {
            if !heap_ready {
                req_size = (req_size + MIN_HEAP_SIZE - 1) & !(MIN_HEAP_SIZE - 1);
            }
            let heap_addr = match self.alloc_pages(
                req_size / PAGE_SIZE_4K,
                PAGE_SIZE_4K,
                UsageKind::RustHeap,
            ) {
                Ok(addr) => addr,
                Err(err) => {
                    req_size /= 2;
                    if req_size < min_size {
                        return Err(err);
                    }
                    continue;
                }
            };
            debug!(
                "expand heap memory: [{:#x}, {:#x})",
                heap_addr,
                heap_addr + req_size
            );
            if heap_ready {
                balloc.add_region(heap_addr, req_size)?;
            } else {
                balloc.init_region(heap_addr, req_size);
                self.heap_ready.store(true, Ordering::Release);
            }
            return Ok(());
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
        self.record_usage_alloc(UsageKind::RustHeap, pages_to_bytes(pages));
        // SAFETY: `addr` is non-null and properly aligned.
        Ok(unsafe { NonNull::new_unchecked(addr as *mut u8) })
    }

    /// Gives back the allocated region to the byte allocator.
    ///
    /// The region should be allocated by [`alloc`], and `align_pow2` should be
    /// the same as the one used in [`alloc`]. Otherwise, the behavior is
    /// undefined.
    ///
    /// [`alloc`]: GlobalAllocator::alloc
    pub fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        // Large allocations bypassed the byte allocator.
        if layout.size() > LARGE_ALLOC_THRESHOLD_BYTES {
            let pages = layout.size().div_ceil(PAGE_SIZE_4K);
            // dealloc_pages handles usages tracking internally.
            self.dealloc_pages(ptr.as_ptr() as usize, pages, UsageKind::RustHeap);
        } else {
            self.record_usage_dealloc(UsageKind::RustHeap, layout.size());

            #[cfg(feature = "slab")]
            if let Some(size_class) = alloc_engine::SizeClass::from_layout(layout) {
                let mut local_owner = AllocLocalOwner::acquire();
                // SAFETY: every eligible allocation is obtained from the
                // central allocator with this class's canonical layout. Local
                // IRQs stay disabled until the cache operation completes.
                if unsafe { slab_cache::try_free(&mut local_owner, size_class, ptr) } {
                    return;
                }

                let mut balloc = self.balloc.lock();
                // SAFETY: the pointer has the canonical class provenance
                // described above, local IRQs remain disabled, and `balloc`
                // is the exclusively locked central allocator.
                unsafe { slab_cache::drain_cache(&mut local_owner, size_class, &mut *balloc, ptr) };
                return;
            }

            self.balloc.lock().deallocate(ptr, layout);
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
        if core::ptr::eq(self, &GLOBAL_ALLOCATOR)
            && (1..=pcp::PCP_MAX_PAGES).contains(&num_pages)
            && align_pow2 <= PAGE_SIZE_4K
        {
            let mut local_owner = AllocLocalOwner::acquire();
            if let Some(addr) = pcp::try_alloc(&mut local_owner, num_pages) {
                drop(local_owner);
                if !matches!(kind, UsageKind::RustHeap) {
                    self.record_usage_alloc(kind, pages_to_bytes(num_pages));
                }
                return Ok(addr);
            }
            drop(local_owner);

            // Cache miss: batch-fill from the global allocator.
            //
            // Acquire the owner before `palloc`: it stores the pre-lock IRQ
            // state, so nesting it inside the `SpinNoIrq` guard would restore
            // "IRQs disabled" when it drops after `palloc` already re-enabled
            // them, leaking an IRQ-off window back to the caller.
            let mut local_owner = AllocLocalOwner::acquire();
            let mut palloc = self.palloc.lock();
            let result = pcp::fill_cache(&mut local_owner, num_pages, &mut palloc);
            drop(palloc);
            if result.is_ok() && !matches!(kind, UsageKind::RustHeap) {
                self.record_usage_alloc(kind, pages_to_bytes(num_pages));
            }
            return result;
        }

        // Slow path: multi-page or special alignment.
        let addr = self.palloc.lock().allocate_pages(num_pages, align_pow2)?;
        if !matches!(kind, UsageKind::RustHeap) {
            self.record_usage_alloc(kind, pages_to_bytes(num_pages));
        }
        Ok(addr)
    }

    /// Allocates one base page with a shared lifetime reference.
    ///
    /// Requires descriptor-backed initialization. Every category is charged
    /// one page until the last reference ends; cloning does not charge again.
    /// The returned page is uninitialized and has not been published to any
    /// other user. This does not allocate a separate metadata control block.
    ///
    /// # Errors
    ///
    /// Returns `NotInitialized` before descriptor-backed handoff, or `NoMemory`
    /// when page inventory cannot satisfy the request. No reclaim is attempted.
    pub fn alloc_frame(&self, kind: UsageKind) -> AllocResult<FrameRef<'_>> {
        if !self.page_ready.load(Ordering::Acquire) {
            return Err(AllocError::NotInitialized);
        }
        self.frames.get().ok_or(AllocError::NotInitialized)?;
        let paddr = buddy_frame_adapter::allocate_frame(self, kind)?;
        // SAFETY: alloc_pages returned exclusive one-page storage from this
        // allocator. It is still unpublished and has no other live owner.
        let frame = unsafe { FrameRef::from_allocation(self, paddr, kind) };
        if matches!(kind, UsageKind::RustHeap) {
            // Raw page backing skips RustHeap accounting because byte callers
            // account their layouts; a FrameRef directly owns the whole page.
            self.record_usage_alloc(kind, PAGE_SIZE_4K);
        }
        Ok(frame)
    }

    /// Allocates at least num_pages contiguous base pages with lifetime holds.
    ///
    /// The returned FrameRun owns every page in the power-of-two backing block,
    /// including rounding slack. All pages are accounted until their own last
    /// holds end. No separate control block or reference array is allocated.
    /// Page contents are uninitialized. This is not a compound-page API.
    ///
    /// # Errors
    /// Returns NotInitialized before descriptor-backed handoff, InvalidInput
    /// for zero/overflowing counts, invalid alignment or a current backend
    /// mapping that cannot preserve physical alignment, and NoMemory when the
    /// backing cannot be obtained. No reclaim is attempted.
    pub fn alloc_frame_run(
        &self,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<FrameRun<'_>> {
        self.alloc_frame_run_below(num_pages, align_pow2, kind, None)
    }

    /// Allocates a lifetime-managed contiguous run entirely below 4 GiB.
    ///
    /// The bound covers the complete power-of-two backing, including slack.
    /// Physical alignment and reference accounting follow alloc_frame_run.
    /// Selection bypasses PCP as the old DMA path did. The current buddy does
    /// not search a DMA zone: an out-of-range candidate returns NoMemory.
    ///
    /// # Errors
    /// Returns the validation/allocation errors of alloc_frame_run, and NoMemory
    /// if the selected backing crosses the exclusive 4 GiB physical bound.
    pub fn alloc_dma_frame_run(
        &self,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
    ) -> AllocResult<FrameRun<'_>> {
        self.alloc_frame_run_below(num_pages, align_pow2, kind, Some(0x1_0000_0000))
    }

    fn alloc_frame_run_below(
        &self,
        num_pages: usize,
        align_pow2: usize,
        kind: UsageKind,
        max_paddr_exclusive: Option<usize>,
    ) -> AllocResult<FrameRun<'_>> {
        if !self.page_ready.load(Ordering::Acquire) || self.frames.get().is_none() {
            return Err(AllocError::NotInitialized);
        }
        if num_pages == 0 || !align_pow2.is_power_of_two() || align_pow2 < PAGE_SIZE_4K {
            return Err(AllocError::InvalidInput);
        }
        let num_frames = num_pages
            .checked_next_power_of_two()
            .ok_or(AllocError::InvalidInput)?;
        let bytes = num_frames
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(AllocError::InvalidInput)?;
        if bytes > isize::MAX as usize {
            return Err(AllocError::InvalidInput);
        }
        let paddr = buddy_frame_adapter::allocate_run(
            self,
            num_frames,
            align_pow2,
            kind,
            max_paddr_exclusive,
        )?;
        // SAFETY: the adapter returned the complete exclusive allocation and
        // validated every base-page descriptor before any owner was published.
        let run = unsafe { FrameRun::from_allocation(self, paddr, num_frames, kind) };
        if matches!(kind, UsageKind::RustHeap) {
            self.record_usage_alloc(kind, bytes);
        }
        Ok(run)
    }

    /// Allocates one zero-filled base page with a shared lifetime reference.
    pub fn alloc_zeroed_frame(&self, kind: UsageKind) -> AllocResult<FrameRef<'_>> {
        let frame = self.alloc_frame(kind)?;
        // SAFETY: this newly allocated page has one unpublished owner and no
        // content aliases. Its stable mapping covers exactly one writable page.
        unsafe { core::ptr::write_bytes(frame.kernel_addr().as_mut_ptr(), 0, PAGE_SIZE_4K) };
        Ok(frame)
    }

    /// Consumes a previously transferred raw reference without incrementing it.
    ///
    /// # Safety
    ///
    /// `paddr` must own exactly one unreconstructed reference from
    /// `FrameRef::into_raw` on this static allocator and allocation instance.
    /// The reference must not have been freed, and required mapping/device
    /// completion must precede dropping the reconstructed owner. A bare PFN,
    /// raw `alloc_pages` address or descriptor lookup does not meet this contract.
    pub unsafe fn frame_from_raw(&'static self, paddr: memaddr::PhysAddr) -> FrameRef<'static> {
        // SAFETY: the caller owns the exact outstanding reference transferred
        // by into_raw on this static allocator and consumes it only once.
        unsafe { FrameRef::from_transferred_reference(self, paddr) }
    }

    /// Restores a transferred run without changing any reference counts.
    ///
    /// # Safety
    /// The PA and actual backing count must identify one outstanding transfer
    /// from FrameRun::into_raw on this allocator. Every page must belong to the
    /// same live allocation incarnation. Restoration consumes that transfer
    /// exactly once; required hardware-use retirement precedes final release.
    pub unsafe fn frame_run_from_raw(
        &'static self,
        paddr: memaddr::PhysAddr,
        num_frames: usize,
    ) -> FrameRun<'static> {
        // SAFETY: the caller owns the complete outstanding run transfer from
        // this static allocator and consumes exactly those page holds once.
        unsafe { FrameRun::from_transferred_reference(self, paddr, num_frames) }
    }

    fn validate_raw_page_release(&self, va: usize, num_pages: usize) -> AllocResult {
        buddy_frame_adapter::validate_raw_release(self, va, num_pages)
    }

    /// Completes one physical frame's retirement through the current backend.
    fn release_frame(&self, paddr: memaddr::PhysAddr, kind: UsageKind) {
        buddy_frame_adapter::release_frame(self, paddr, kind);
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
            self.record_usage_alloc(kind, pages_to_bytes(num_pages));
        }
        Ok(addr)
    }

    /// Gives back the allocated pages starts from `va` to the page allocator.
    ///
    /// The pages should be allocated by [`alloc_pages`], and `align_pow2`
    /// should be the same as the one used in [`alloc_pages`]. Otherwise, the
    /// behavior is undefined.
    ///
    /// In descriptor-backed allocators, every slot is checked before accounting
    /// or inventory mutation. Referenced and permanent pages must not be freed
    /// through this raw interface; misuse panics. This check does not prove raw
    /// allocation provenance or detect all stale-address/double frees.
    ///
    /// 1–4 page deallocations are cached per-CPU to reduce global lock
    /// contention.
    ///
    /// [`alloc_pages`]: GlobalAllocator::alloc_pages
    pub fn dealloc_pages(&self, va: usize, num_pages: usize, kind: UsageKind) {
        self.validate_raw_page_release(va, num_pages)
            .expect("raw deallocation cannot return a referenced or permanent frame");
        self.dealloc_pages_inner(va, num_pages, kind);
    }

    fn dealloc_pages_inner(&self, va: usize, num_pages: usize, kind: UsageKind) {
        self.record_usage_dealloc(kind, pages_to_bytes(num_pages));

        // Fast path: 1–4 pages -> try to cache them per-CPU.
        if core::ptr::eq(self, &GLOBAL_ALLOCATOR) && (1..=pcp::PCP_MAX_PAGES).contains(&num_pages) {
            let mut local_owner = AllocLocalOwner::acquire();
            if pcp::try_free(&mut local_owner, num_pages, va) {
                return;
            }
            drop(local_owner);

            // Cache full: bulk-drain to the global allocator. Acquire the owner
            // before `palloc` for the same IRQ-state nesting reason as
            // `alloc_pages`.
            let mut local_owner = AllocLocalOwner::acquire();
            let mut palloc = self.palloc.lock();
            pcp::drain_cache(&mut local_owner, num_pages, &mut palloc, va);
            return;
        }

        // Slow path: multi-page.
        self.palloc.lock().deallocate_pages(va, num_pages);
    }

    /// Add diagnostic usage without placing allocation paths behind a usage lock.
    fn record_usage_alloc(&self, kind: UsageKind, size_bytes: usize) {
        with_current_usage_counters(|counters| counters.alloc(kind, size_bytes));
    }

    /// Record freed bytes for aggregate diagnostic accounting.
    fn record_usage_dealloc(&self, kind: UsageKind, size_bytes: usize) {
        with_current_usage_counters(|counters| counters.dealloc(kind, size_bytes));
    }

    fn init_or_extend_page_allocator(&self, va: usize, size: usize) -> AllocResult {
        let mut pages = self.palloc.lock();
        if self.frames.get().is_some() {
            return Err(AllocError::AlreadyInitialized);
        }
        if self.page_ready.load(Ordering::Acquire) {
            pages.add_region(va, size)?;
        } else {
            pages.init_region(va, size);
            self.page_ready.store(true, Ordering::Release);
        }
        Ok(())
    }

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

    // Note: The following delegation pattern is a standard Rust idiom for
    // thread-safe interior mutability, not a literal duplication.
    /// Returns the number of allocated bytes in the byte allocator.
    pub fn used_bytes(&self) -> usize {
        self.balloc.lock().used_bytes()
    }

    /// Returns the number of available bytes in the byte allocator.
    pub fn available_bytes(&self) -> usize {
        self.balloc.lock().available_bytes()
    }

    /// Returns the total number of allocatable pages managed by the page allocator.
    ///
    /// The total excludes allocator metadata such as the buddy free-head bitmap.
    pub fn total_pages(&self) -> usize {
        self.palloc.lock().total_pages()
    }

    /// Returns the number of allocated pages in the page allocator.
    pub fn used_pages(&self) -> usize {
        self.palloc.lock().used_pages()
    }

    /// Returns the number of available pages in the page allocator.
    pub fn available_pages(&self) -> usize {
        self.palloc.lock().available_pages()
    }

    /// Returns caller-live usage aggregated from all per-CPU counters.
    ///
    /// The snapshot is not linearized against concurrent allocation and free;
    /// it may temporarily overstate or understate usage, but it is never used
    /// for allocator ownership or reclaim decisions.
    ///
    /// # Panics
    ///
    /// Panics if cumulative accounting overflows or total recorded frees
    /// exceed total recorded allocations for any [`UsageKind`].
    pub fn usages(&self) -> Usages {
        usage_snapshot()
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

/// Hands all boot RAM to the global allocator with resident page descriptors.
///
/// Metadata is prepared before page allocation becomes ready, then the byte
/// heap is bootstrapped from the remaining inventory. A heap-bootstrap error
/// leaves the page allocator initialized; callers must not repeat the handoff.
///
/// # Safety
///
/// Regions must be exclusive writable RAM at permanently valid linear kernel
/// mappings. This is a one-time boot operation, before other allocator users.
/// The storage must remain valid for the lifetime of the kernel.
pub unsafe fn global_init_with_regions(regions: &[FrameRegion]) -> AllocResult<FrameBootstrapInfo> {
    // SAFETY: the caller transfers exclusive, permanently mapped boot RAM and
    // guarantees that allocator initialization is serialized.
    let info = unsafe { GLOBAL_ALLOCATOR.init_page_allocator_with_regions(regions) }?;
    GLOBAL_ALLOCATOR.bootstrap_heap_if_needed()?;
    Ok(info)
}

/// Initializes the global allocator with the given memory region.
///
/// The allocator writes free-list and bitmap metadata into the region during
/// registration, so it must already be writable, mapped, and exclusively owned.
/// Use [`global_init_with_regions`] for the kernel's descriptor-backed handoff.
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

/// Return whether the global backing page allocator is ready for requests.
pub fn is_page_allocator_ready() -> bool {
    GLOBAL_ALLOCATOR.is_page_allocator_ready()
}

unittest::path_mod_test!(pub mod tests_kalloc, "../ut-tests/lib.rs");

unittest::path_mod_test!(tests_frame_run, "../ut-tests/frame_run.rs");

unittest::path_mod_test!(tests_global_page, "../ut-tests/global_page.rs");
