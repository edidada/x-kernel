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

use alloc_engine::{AllocResult, BaseAllocator, ByteAllocator};
use kaddr_layout::v2p;
use kcpu_slot::{CpuId, CpuPin, cpu_slot_cell};
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

fn kernel_virt_to_phys(vaddr: usize) -> usize {
    v2p(vaddr)
}

mod page;
mod pages;
pub use pages::{PhysicalPageAllocator, is_page_allocator_ready, page_allocator};
mod pcp;
#[cfg(feature = "slab")]
mod slab_cache;
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

/// Kernel object heap backed by the independent physical-page service.
///
/// Owns byte allocation and heap readiness. The selected byte allocator serves
/// objects; large requests use the page service directly. Slab configurations add
/// a per-CPU object cache. System page inventory belongs to page_allocator().
pub struct GlobalAllocator {
    balloc: SpinNoIrq<DefaultByteAllocator>,
    /// Whether the byte allocator already owns a bootstrap heap region.
    heap_ready: AtomicBool,
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
            record_usage_alloc(UsageKind::RustHeap, layout.size());
            return Ok(object);
        }

        let object = self.alloc_from_byte_allocator(layout)?;
        record_usage_alloc(UsageKind::RustHeap, layout.size());
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
            let heap_paddr = match page_allocator()
                .alloc_from_inventory(req_size / PAGE_SIZE_4K, PAGE_SIZE_4K)
            {
                Ok(addr) => addr,
                Err(err) => {
                    req_size /= 2;
                    if req_size < min_size {
                        return Err(err);
                    }
                    continue;
                }
            };
            let heap_addr = kaddr_layout::p2v(heap_paddr.as_usize());
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
        let paddr = page_allocator()
            .alloc_pages(pages, align, UsageKind::RustHeap)
            .inspect_err(|e| {
                error!(
                    "large_alloc: failed to allocate {} pages ({} bytes): {:?}",
                    pages,
                    layout.size(),
                    e
                );
            })?;
        let addr = kaddr_layout::p2v(paddr.as_usize());
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
            page_allocator().dealloc_pages(
                kernel_virt_to_phys(ptr.as_ptr() as usize).into(),
                pages,
                UsageKind::RustHeap,
            );
        } else {
            record_usage_dealloc(UsageKind::RustHeap, layout.size());

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

    fn bootstrap_heap_if_needed(&self) -> AllocResult {
        if self.heap_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let paddr =
            page_allocator().alloc_from_inventory(MIN_HEAP_SIZE / PAGE_SIZE_4K, PAGE_SIZE_4K)?;
        let heap_addr = kaddr_layout::p2v(paddr.as_usize());
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
}

fn record_usage_alloc(kind: UsageKind, size_bytes: usize) {
    with_current_usage_counters(|counters| counters.alloc(kind, size_bytes));
}

fn record_usage_dealloc(kind: UsageKind, size_bytes: usize) {
    with_current_usage_counters(|counters| counters.dealloc(kind, size_bytes));
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
pub fn usages() -> Usages {
    usage_snapshot()
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

/// Returns the shared kernel object heap. Page users call page_allocator().
pub fn global_allocator() -> &'static GlobalAllocator {
    &GLOBAL_ALLOCATOR
}

/// Initializes the kernel object heap after boot has registered physical RAM.
///
/// Call once during serialized boot before ordinary heap users run. This does
/// not initialize or own page inventory.
///
/// # Errors
/// Returns a page allocation error if initial backing cannot be obtained.
pub fn init_heap() -> AllocResult {
    GLOBAL_ALLOCATOR.bootstrap_heap_if_needed()
}

unittest::path_mod_test!(pub mod tests_kalloc, "../ut-tests/lib.rs");
