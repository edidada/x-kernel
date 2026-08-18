// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Slab-only central caches and hardirq reserves for the kernel allocator.
//!
//! Each [`SizeClass`] owns one central [`SlabCache`] plus a small independent
//! hardirq reserve. Task-context magazines exchange objects with the central
//! cache one object per short lock acquisition; page allocation and span
//! initialization always remain outside this module's locks.

use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use alloc_engine::{
    AllocResult, PreparedSlab, SIZE_CLASS_COUNT, SizeClass, SlabBacking, SlabCache,
};
use kbuild_config::NR_CPUS;
use kspin::{NoPreempt, SpinNoIrq};
use memaddr::PAGE_SIZE_4K;

use super::magazine::{ObjectBatch, PerCpuMagazineSet};

/// Runtime task magazines associated with the global [`KmemCacheSet`].
///
/// Only `KmemCacheSet` exposes accessors for this storage, keeping the
/// per-CPU policy within the slab-specific cache layer rather than making it
/// a generic global allocator facility.
#[percpu::def_percpu]
static PER_CPU_MAGAZINES: PerCpuMagazineSet = PerCpuMagazineSet::new();

/// Per-CPU access state for the task magazine pair set.
///
/// A reclaim initiator changes an idle or active target into one of the two
/// pending states before sending its IPI.  The target IPI handler may complete
/// only an idle-origin request; an active-origin request is completed by the
/// task after it has released its mutable per-CPU magazine borrow.  This
/// distinction is what makes the later remote mutable borrow sound.
#[percpu::def_percpu]
static PER_CPU_MAGAZINE_FLUSH_STATE: AtomicU64 = AtomicU64::new(MAGAZINE_STATE_IDLE);

const MAGAZINE_STATE_BITS: u64 = 3;
const MAGAZINE_STATE_TAG_MASK: u64 = (1 << MAGAZINE_STATE_BITS) - 1;
const MAGAZINE_STATE_IDLE: u64 = 0;
const MAGAZINE_STATE_ACTIVE: u64 = 1;
const MAGAZINE_STATE_PENDING_IDLE: u64 = 2;
const MAGAZINE_STATE_PENDING_ACTIVE: u64 = 3;
const MAGAZINE_STATE_FROZEN: u64 = 4;

/// Largest sequence that fits in a task-magazine flush state word.
pub const KMEM_CACHE_FLUSH_SEQUENCE_MAX: u64 = u64::MAX >> MAGAZINE_STATE_BITS;

#[derive(Clone, Copy)]
pub(crate) enum TaskMagazineAccess<T> {
    Ready(T),
    Frozen,
}

#[derive(Clone, Copy)]
pub(crate) enum TaskMagazineRefill {
    Installed(ObjectBatch),
    Frozen(ObjectBatch),
}

#[inline]
const fn magazine_state_tag(state: u64) -> u64 {
    state & MAGAZINE_STATE_TAG_MASK
}

#[inline]
const fn magazine_state_sequence(state: u64) -> u64 {
    state >> MAGAZINE_STATE_BITS
}

#[inline]
const fn magazine_state_with_sequence(tag: u64, sequence: u64) -> u64 {
    debug_assert!(sequence != 0);
    debug_assert!(sequence <= KMEM_CACHE_FLUSH_SEQUENCE_MAX);
    (sequence << MAGAZINE_STATE_BITS) | tag
}

/// Per-class central state protected by the class's IRQ-safe lock.
struct KmemCache {
    slab: SlabCache<PAGE_SIZE_4K>,
    irq_reserve: ObjectBatch,
}

impl KmemCache {
    const fn new(size_class: SizeClass) -> Self {
        Self {
            slab: SlabCache::new(size_class),
            irq_reserve: ObjectBatch::new(),
        }
    }
}

/// Central kernel caches, one for each slab size class.
///
/// The locks protect only class metadata transitions, one-object freelist
/// operations, and the small hardirq reserve. Callers must prepare backing
/// spans and return detached pages outside these locks.
pub(crate) struct KmemCacheSet {
    caches: [SpinNoIrq<KmemCache>; SIZE_CLASS_COUNT],
    irq_reserve_needs_refill: [AtomicBool; SIZE_CLASS_COUNT],
}

impl KmemCacheSet {
    /// Create empty central caches for every [`SizeClass`].
    pub const fn new() -> Self {
        Self {
            caches: [
                SpinNoIrq::new(KmemCache::new(SizeClass::Bytes8)),
                SpinNoIrq::new(KmemCache::new(SizeClass::Bytes16)),
                SpinNoIrq::new(KmemCache::new(SizeClass::Bytes32)),
                SpinNoIrq::new(KmemCache::new(SizeClass::Bytes64)),
                SpinNoIrq::new(KmemCache::new(SizeClass::Bytes128)),
                SpinNoIrq::new(KmemCache::new(SizeClass::Bytes256)),
                SpinNoIrq::new(KmemCache::new(SizeClass::Bytes512)),
                SpinNoIrq::new(KmemCache::new(SizeClass::Bytes1024)),
                SpinNoIrq::new(KmemCache::new(SizeClass::Bytes2048)),
            ],
            irq_reserve_needs_refill: [const { AtomicBool::new(true) }; SIZE_CLASS_COUNT],
        }
    }

    /// Try to allocate an already available object from `size_class`'s
    /// central cache.
    ///
    /// Returns `None` without allocating backing pages when the central cache
    /// is empty. It deliberately does not consume the hardirq reserve.
    #[inline]
    pub(crate) fn try_allocate(&self, size_class: SizeClass) -> Option<NonNull<u8>> {
        self.cache(size_class).lock().slab.allocate()
    }

    /// Transfer up to `target_count` central objects into a task refill batch.
    ///
    /// Every object transfer takes and releases the class lock separately, so
    /// the task remains IRQ-enabled between transfers. No page allocator work
    /// happens here.
    pub(crate) fn take_task_batch(
        &self,
        size_class: SizeClass,
        target_count: usize,
        batch: &mut ObjectBatch,
    ) {
        while batch.len() < target_count {
            let Some(object) = self.try_allocate(size_class) else {
                break;
            };
            batch.push(object);
        }
    }

    /// Try to allocate from the current CPU's task-context magazine.
    ///
    /// A [`TaskMagazineAccess::Frozen`] result means a synchronized reclaim is
    /// draining this CPU's magazines.  The caller must use the central cache
    /// for that operation instead of waiting in the allocation fast path.
    ///
    /// # Safety
    ///
    /// The caller must have routed hardirq execution to this cache set's IRQ
    /// reserve and must not invoke this path from NMI context.
    #[inline]
    pub(crate) unsafe fn try_allocate_from_task_magazine(
        &self,
        size_class: SizeClass,
    ) -> TaskMagazineAccess<Option<NonNull<u8>>> {
        self.with_current_task_magazines(|magazines| magazines.try_allocate(size_class))
    }

    /// Cache a task-context free, returning a full magazine for central drain.
    ///
    /// A frozen CPU leaves `object` untouched in the caller's ownership.  The
    /// caller must return it through the matching central cache.
    ///
    /// # Safety
    ///
    /// The caller must meet the execution-context contract of
    /// [`try_allocate_from_task_magazine`](Self::try_allocate_from_task_magazine),
    /// and `object` must be a live allocation in `size_class` from this cache
    /// set.
    #[inline]
    pub(crate) unsafe fn deallocate_to_task_magazine(
        &self,
        size_class: SizeClass,
        object: NonNull<u8>,
    ) -> TaskMagazineAccess<Option<ObjectBatch>> {
        self.with_current_task_magazines(|magazines| magazines.deallocate(size_class, object))
    }

    /// Install a central refill batch into the current CPU's task magazine.
    ///
    /// Returns a frozen result with the original batch when reclaim started
    /// while the caller was acquiring objects centrally.  The caller must
    /// drain that batch through the central cache instead.
    ///
    /// # Safety
    ///
    /// The caller must meet the execution-context contract of
    /// [`try_allocate_from_task_magazine`](Self::try_allocate_from_task_magazine),
    /// and `batch` must contain only objects from this cache set's
    /// `size_class`.
    #[inline]
    pub(crate) unsafe fn install_task_refill(
        &self,
        size_class: SizeClass,
        mut batch: ObjectBatch,
    ) -> TaskMagazineRefill {
        match self.with_current_task_magazines(|magazines| {
            magazines.install_refill(size_class, &mut batch);
        }) {
            TaskMagazineAccess::Ready(()) => TaskMagazineRefill::Installed(batch),
            TaskMagazineAccess::Frozen => TaskMagazineRefill::Frozen(batch),
        }
    }

    /// Return a task magazine batch to its central cache.
    ///
    /// # Safety
    ///
    /// Every batch entry must be a live allocation from this cache set's
    /// matching `size_class`, and must not have been deallocated before.
    pub(crate) unsafe fn return_task_batch(&self, size_class: SizeClass, mut batch: ObjectBatch) {
        while let Some(object) = batch.pop() {
            // SAFETY: the caller's batch contract is forwarded one object at
            // a time to the matching central class.
            unsafe { self.deallocate(size_class, object) };
        }
    }

    /// Return an object to its class's central cache.
    ///
    /// # Safety
    ///
    /// `object` must be a live allocation from this cache set's matching
    /// `size_class`, and must not have been deallocated before.
    #[inline]
    pub(crate) unsafe fn deallocate(&self, size_class: SizeClass, object: NonNull<u8>) {
        let mut cache = self.cache(size_class).lock();
        // SAFETY: the caller's object-lifetime contract is forwarded to the
        // class cache while its metadata lock is held.
        unsafe { cache.slab.deallocate(object) };
        self.set_irq_reserve_needs_refill(
            size_class,
            cache.irq_reserve.len() < irq_reserve_capacity(size_class),
        );
    }

    /// Try to donate a task-context free to the hardirq reserve.
    ///
    /// This is only called when the reserve's atomic hint says it needs an
    /// object. A false return means the reserve was filled concurrently and
    /// the caller should use its normal task magazine instead.
    ///
    /// # Safety
    ///
    /// `object` must satisfy the same live-object contract as
    /// [`deallocate`](Self::deallocate).
    pub(crate) unsafe fn try_deallocate_to_irq_reserve(
        &self,
        size_class: SizeClass,
        object: NonNull<u8>,
    ) -> bool {
        let mut cache = self.cache(size_class).lock();
        if cache.irq_reserve.len() == irq_reserve_capacity(size_class) {
            self.set_irq_reserve_needs_refill(size_class, false);
            return false;
        }
        cache.irq_reserve.push(object);
        self.set_irq_reserve_needs_refill(
            size_class,
            cache.irq_reserve.len() < irq_reserve_capacity(size_class),
        );
        true
    }

    /// Try to allocate from the hardirq reserve without touching task
    /// magazines, page allocation, or span initialization.
    #[inline]
    pub(crate) fn try_allocate_irq(&self, size_class: SizeClass) -> Option<NonNull<u8>> {
        let mut cache = self.cache(size_class).lock();
        let object = cache.irq_reserve.pop();
        self.set_irq_reserve_needs_refill(
            size_class,
            cache.irq_reserve.len() < irq_reserve_capacity(size_class),
        );
        object
    }

    /// Allocate one object for hardirq execution.
    ///
    /// The independent reserve is always preferred. If it is empty, consume
    /// one object already present on the matching central free list. This
    /// bounded fallback never prepares a slab, allocates pages, or touches a
    /// task magazine.
    pub(crate) fn try_allocate_irq_or_central(&self, size_class: SizeClass) -> Option<NonNull<u8>> {
        self.try_allocate_irq(size_class)
            .or_else(|| self.try_allocate(size_class))
    }

    /// Return an object from hardirq execution to the independent reserve.
    ///
    /// If the reserve is already full, the object returns to the matching
    /// central slab cache. Both cases are bounded, metadata-only work.
    ///
    /// # Safety
    ///
    /// `object` must satisfy the same live-object contract as
    /// [`deallocate`](Self::deallocate).
    pub(crate) unsafe fn deallocate_irq(&self, size_class: SizeClass, object: NonNull<u8>) {
        let mut cache = self.cache(size_class).lock();
        if cache.irq_reserve.len() < irq_reserve_capacity(size_class) {
            cache.irq_reserve.push(object);
            self.set_irq_reserve_needs_refill(
                size_class,
                cache.irq_reserve.len() < irq_reserve_capacity(size_class),
            );
            return;
        }

        // SAFETY: the caller's object-lifetime contract is forwarded to the
        // class cache while its metadata lock is held.
        unsafe { cache.slab.deallocate(object) };
        self.set_irq_reserve_needs_refill(size_class, false);
    }

    /// Refill the hardirq reserve from already-central objects when needed.
    ///
    /// A normal task calls this after a local magazine miss. Each transfer has
    /// an independent short class lock; the caller stays IRQ-enabled between
    /// transfers. The method never allocates backing pages.
    pub(crate) fn replenish_irq_reserve(&self, size_class: SizeClass) {
        if !self.irq_reserve_needs_refill(size_class) {
            return;
        }

        loop {
            let should_continue = {
                let mut cache = self.cache(size_class).lock();
                let capacity = irq_reserve_capacity(size_class);
                if cache.irq_reserve.len() == capacity {
                    self.set_irq_reserve_needs_refill(size_class, false);
                    false
                } else if let Some(object) = cache.slab.allocate() {
                    cache.irq_reserve.push(object);
                    let still_needs_refill = cache.irq_reserve.len() < capacity;
                    self.set_irq_reserve_needs_refill(size_class, still_needs_refill);
                    still_needs_refill
                } else {
                    // Keep the under-full state visible so a normal task free
                    // can donate directly to the reserve. Callers invoke this
                    // only on a local miss, so an empty central cache does not
                    // make every magazine hit take a class lock.
                    self.set_irq_reserve_needs_refill(size_class, true);
                    false
                }
            };
            if !should_continue {
                return;
            }
        }
    }

    /// Return whether a normal task should attempt a reserve refill.
    #[inline]
    pub(crate) fn irq_reserve_needs_refill(&self, size_class: SizeClass) -> bool {
        self.irq_reserve_needs_refill[size_class.index()].load(Ordering::Acquire)
    }

    /// Prepare a freshly allocated backing span before taking a central lock.
    ///
    /// # Safety
    ///
    /// `base` must be an exclusive, writable span with the page count and
    /// alignment returned by [`slab_pages`](Self::slab_pages) and
    /// [`slab_bytes`](Self::slab_bytes) for `size_class`.
    pub(crate) unsafe fn prepare_slab(
        size_class: SizeClass,
        base: NonNull<u8>,
    ) -> AllocResult<PreparedSlab<PAGE_SIZE_4K>> {
        // SAFETY: this method has exactly the backing-span contract required
        // by `SlabCache::prepare_slab`.
        unsafe { SlabCache::prepare_slab(size_class, base) }
    }

    /// Publish a prepared span under its class's short metadata lock.
    ///
    /// Returns the input token if it cannot be published; callers must return
    /// the token's backing pages outside the lock.
    pub(crate) fn publish_slab(
        &self,
        size_class: SizeClass,
        slab: PreparedSlab<PAGE_SIZE_4K>,
    ) -> Result<(), PreparedSlab<PAGE_SIZE_4K>> {
        let mut cache = self.cache(size_class).lock();
        let result = cache.slab.publish_slab(slab);
        if result.is_ok() {
            self.set_irq_reserve_needs_refill(
                size_class,
                cache.irq_reserve.len() < irq_reserve_capacity(size_class),
            );
        }
        result
    }

    /// Return whether `size_class` has a central object ready for a task
    /// refill. The hardirq reserve is intentionally excluded.
    pub(crate) fn has_available_object(&self, size_class: SizeClass) -> bool {
        self.cache(size_class).lock().slab.free_blocks() != 0
    }

    /// Detach one empty span from any size class.
    ///
    /// The caller must return the result to its page allocator outside this
    /// method and outside all central-cache locks.
    pub(crate) fn take_empty_slab(&self) -> Option<SlabBacking> {
        for cache in &self.caches {
            if let Some(backing) = cache.lock().slab.take_empty_slab() {
                return Some(backing);
            }
        }
        None
    }

    /// Number of backing pages required by one span of `size_class`.
    pub(crate) const fn slab_pages(size_class: SizeClass) -> usize {
        size_class.slab_pages(PAGE_SIZE_4K)
    }

    /// Alignment and byte size required by one span of `size_class`.
    pub(crate) const fn slab_bytes(size_class: SizeClass) -> usize {
        Self::slab_pages(size_class) * PAGE_SIZE_4K
    }

    /// Bytes checked out from central slab free lists, excluding hardirq
    /// reserve objects but including task-local magazine objects.
    pub(crate) fn used_bytes(&self) -> usize {
        self.caches.iter().fold(0usize, |bytes, cache| {
            let cache = cache.lock();
            let reserve_bytes = cache
                .irq_reserve
                .len()
                .saturating_mul(cache.slab.size_class().size());
            bytes.saturating_add(cache.slab.used_bytes().saturating_sub(reserve_bytes))
        })
    }

    /// Centrally free and hardirq-reserve object bytes, rounded to class size.
    ///
    /// Task-local magazine objects are intentionally not included: obtaining a
    /// synchronized cross-CPU view is the responsibility of the flush
    /// protocol, not this fast diagnostic accessor.
    pub(crate) fn available_bytes(&self) -> usize {
        self.caches.iter().fold(0usize, |bytes, cache| {
            let cache = cache.lock();
            let class_size = cache.slab.size_class().size();
            let central_bytes = cache.slab.free_blocks().saturating_mul(class_size);
            let reserve_bytes = cache.irq_reserve.len().saturating_mul(class_size);
            bytes.saturating_add(central_bytes.saturating_add(reserve_bytes))
        })
    }

    /// Move every hardirq-reserve object back to its central slab cache.
    ///
    /// This is a reclaim-only operation.  It moves one object under one short
    /// class lock at a time, so it does not turn a bounded reserve into a long
    /// IRQ-off critical section.
    pub(crate) fn drain_irq_reserves(&self) {
        for size_class in SizeClass::ALL {
            loop {
                let moved = {
                    let mut cache = self.cache(size_class).lock();
                    match cache.irq_reserve.pop() {
                        Some(object) => {
                            // SAFETY: an IRQ-reserve entry is a live object
                            // checked out from this matching slab cache.
                            // Returning it here transfers it back while the
                            // class metadata lock is held.
                            unsafe { cache.slab.deallocate(object) };
                            self.set_irq_reserve_needs_refill(size_class, true);
                            true
                        }
                        None => {
                            self.set_irq_reserve_needs_refill(size_class, true);
                            false
                        }
                    }
                };
                if !moved {
                    break;
                }
            }
        }
    }

    /// Return whether the current CPU must bypass task magazines for reclaim.
    ///
    /// This is a best-effort pre-check for the task-free path.  The actual
    /// magazine access still performs its own atomic state transition, so an
    /// IPI arriving immediately after this check remains safe.
    pub(crate) fn is_current_task_magazine_frozen(&self) -> bool {
        let _no_preempt = NoPreempt::new();
        PER_CPU_MAGAZINE_FLUSH_STATE.with_current(|state| {
            magazine_state_tag(state.load(Ordering::Acquire)) != MAGAZINE_STATE_IDLE
        })
    }

    /// Request that one CPU stop task-magazine access for `sequence`.
    ///
    /// The caller must use a present logical CPU index and must serialize
    /// sequences globally.  This method only changes atomic state; it never
    /// borrows the target's magazine storage.  The target's IPI handler or a
    /// currently active local access completes the transition to frozen.
    pub(crate) fn request_task_magazine_flush_on_cpu(&self, cpu: usize, sequence: u64) {
        let state = task_magazine_flush_state_on_cpu(cpu);
        request_task_magazine_flush(state, sequence);
    }

    /// Complete a request for the current CPU from the IPI handler.
    ///
    /// A request that found an active task access intentionally remains
    /// pending: the interrupted task owns the mutable magazine reference and
    /// performs the completion after that reference is dropped.
    pub(crate) fn complete_current_task_magazine_flush(&self, sequence: u64) -> bool {
        let _no_preempt = NoPreempt::new();
        PER_CPU_MAGAZINE_FLUSH_STATE
            .with_current(|state| complete_task_magazine_flush(state, sequence))
    }

    /// Return whether one target CPU has completed the exact flush request.
    pub(crate) fn is_task_magazine_flush_complete(&self, cpu: usize, sequence: u64) -> bool {
        if cpu >= NR_CPUS || !is_valid_magazine_flush_sequence(sequence) {
            return false;
        }

        let expected = magazine_state_with_sequence(MAGAZINE_STATE_FROZEN, sequence);
        task_magazine_flush_state_on_cpu(cpu).load(Ordering::Acquire) == expected
    }

    /// Drain all frozen magazines of one CPU through central caches.
    ///
    /// The caller must retain the global flush session until it later calls
    /// [`unfreeze_task_magazines_on_cpu`](Self::unfreeze_task_magazines_on_cpu).
    /// No page allocation or page release occurs here.
    pub(crate) fn drain_frozen_task_magazines_on_cpu(&self, cpu: usize, sequence: u64) -> bool {
        if !self.is_task_magazine_flush_complete(cpu, sequence) {
            return false;
        }

        for size_class in SizeClass::ALL {
            let batches = {
                // SAFETY: `is_task_magazine_flush_complete` observed this
                // CPU in the matching frozen state.  The state machine blocks
                // all new task magazine borrows and only the local task can
                // complete an active borrow before publishing that state.
                // Hardirq never accesses `PER_CPU_MAGAZINES`.  The caller
                // keeps the global flush session active until this drain is
                // complete, so no other CPU can unfreeze this storage.
                unsafe { PER_CPU_MAGAZINES.remote_ref_mut_raw(cpu) }.take_all(size_class)
            };
            for batch in batches {
                // SAFETY: a frozen magazine contains only live objects from
                // its matching class, and `take_all` removes each object
                // exactly once before handing the batch to the central cache.
                unsafe { self.return_task_batch(size_class, batch) };
            }
        }
        true
    }

    /// Unfreeze one CPU after its magazine contents have been drained.
    pub(crate) fn unfreeze_task_magazines_on_cpu(&self, cpu: usize, sequence: u64) {
        let expected = magazine_state_with_sequence(MAGAZINE_STATE_FROZEN, sequence);
        let state = task_magazine_flush_state_on_cpu(cpu);
        let observed = state
            .compare_exchange(
                expected,
                MAGAZINE_STATE_IDLE,
                Ordering::Release,
                Ordering::Acquire,
            )
            .unwrap_or_else(|observed| {
                panic!(
                    "unfreezing task magazines on CPU {cpu} for sequence {sequence}: expected \
                     frozen state {expected:#x}, found {observed:#x}"
                )
            });
        debug_assert_eq!(observed, expected);
    }

    /// Freeze, drain, and later unfreeze only the current CPU.
    ///
    /// This is the no-IPI fallback used when no cross-CPU provider is linked.
    /// A same-CPU active access cannot be waited for here because it would be
    /// a nested reclaim call.  This path therefore accepts only an already
    /// idle magazine state and leaves an active access completely unchanged.
    #[cfg(not(feature = "ipi"))]
    pub(crate) fn freeze_and_drain_current_task_magazines(&self, sequence: u64) -> bool {
        let _no_preempt = NoPreempt::new();
        let frozen = PER_CPU_MAGAZINE_FLUSH_STATE
            .with_current(|state| try_freeze_idle_task_magazines(state, sequence));
        if !frozen {
            return false;
        }

        for size_class in SizeClass::ALL {
            let batches = {
                // SAFETY: the current CPU was transitioned to frozen above
                // while preemption is disabled.  Task access observes that
                // state before borrowing the magazine, and hardirq uses only
                // the separate reserve.
                unsafe { PER_CPU_MAGAZINES.current_ref_mut_raw() }.take_all(size_class)
            };
            for batch in batches {
                // SAFETY: see the matching remote-drain contract above; this
                // batch was removed exactly once from its matching class.
                unsafe { self.return_task_batch(size_class, batch) };
            }
        }
        true
    }

    /// Unfreeze the current CPU after
    /// [`freeze_and_drain_current_task_magazines`](Self::freeze_and_drain_current_task_magazines).
    #[cfg(not(feature = "ipi"))]
    pub(crate) fn unfreeze_current_task_magazines(&self, sequence: u64) {
        let _no_preempt = NoPreempt::new();
        let expected = magazine_state_with_sequence(MAGAZINE_STATE_FROZEN, sequence);
        PER_CPU_MAGAZINE_FLUSH_STATE.with_current(|state| {
            let observed = state
                .compare_exchange(
                    expected,
                    MAGAZINE_STATE_IDLE,
                    Ordering::Release,
                    Ordering::Acquire,
                )
                .unwrap_or_else(|observed| {
                    panic!(
                        "unfreezing current task magazines for sequence {sequence}: expected \
                         frozen state {expected:#x}, found {observed:#x}"
                    )
                });
            debug_assert_eq!(observed, expected);
        });
    }

    fn with_current_task_magazines<T>(
        &self,
        f: impl FnOnce(&mut PerCpuMagazineSet) -> T,
    ) -> TaskMagazineAccess<T> {
        let _no_preempt = NoPreempt::new();
        let entered = PER_CPU_MAGAZINE_FLUSH_STATE.with_current(|state| {
            state
                .compare_exchange(
                    MAGAZINE_STATE_IDLE,
                    MAGAZINE_STATE_ACTIVE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        });
        if !entered {
            return TaskMagazineAccess::Frozen;
        }

        let result = PER_CPU_MAGAZINES.with_current(f);
        self.finish_current_task_magazine_access();
        TaskMagazineAccess::Ready(result)
    }

    fn finish_current_task_magazine_access(&self) {
        PER_CPU_MAGAZINE_FLUSH_STATE.with_current(|state| finish_task_magazine_access(state));
    }

    fn cache(&self, size_class: SizeClass) -> &SpinNoIrq<KmemCache> {
        &self.caches[size_class.index()]
    }

    /// Publish the reserve's under-full state while its class lock is held.
    ///
    /// All writers call this before releasing that lock. This prevents an
    /// older snapshot from overwriting a hardirq's newer reserve transition;
    /// readers use the value only as an opportunistic task-context hint.
    fn set_irq_reserve_needs_refill(&self, size_class: SizeClass, needs_refill: bool) {
        self.irq_reserve_needs_refill[size_class.index()].store(needs_refill, Ordering::Release);
    }
}

fn is_valid_magazine_flush_sequence(sequence: u64) -> bool {
    (1..=KMEM_CACHE_FLUSH_SEQUENCE_MAX).contains(&sequence)
}

fn task_magazine_flush_state_on_cpu(cpu: usize) -> &'static AtomicU64 {
    assert!(
        cpu < NR_CPUS,
        "task magazine CPU index {cpu} exceeds NR_CPUS"
    );
    // SAFETY: `cpu` is bounded by the compile-time per-CPU allocation.  The
    // returned value is atomic; the state machine below provides the separate
    // exclusion needed before any remote mutable magazine borrow.
    unsafe { PER_CPU_MAGAZINE_FLUSH_STATE.remote_ref_raw(cpu) }
}

fn request_task_magazine_flush(state: &AtomicU64, sequence: u64) {
    assert!(
        is_valid_magazine_flush_sequence(sequence),
        "invalid task magazine flush sequence {sequence}"
    );
    let pending_idle = magazine_state_with_sequence(MAGAZINE_STATE_PENDING_IDLE, sequence);
    let pending_active = magazine_state_with_sequence(MAGAZINE_STATE_PENDING_ACTIVE, sequence);

    loop {
        let observed = state.load(Ordering::Acquire);
        let target = match magazine_state_tag(observed) {
            MAGAZINE_STATE_IDLE => pending_idle,
            MAGAZINE_STATE_ACTIVE => pending_active,
            MAGAZINE_STATE_PENDING_IDLE | MAGAZINE_STATE_PENDING_ACTIVE => {
                assert_eq!(
                    magazine_state_sequence(observed),
                    sequence,
                    "overlapping task magazine flush sequences: state={observed:#x}, \
                     requested={sequence}"
                );
                return;
            }
            MAGAZINE_STATE_FROZEN => {
                assert_eq!(
                    magazine_state_sequence(observed),
                    sequence,
                    "overlapping task magazine flush sequences: state={observed:#x}, \
                     requested={sequence}"
                );
                return;
            }
            tag => panic!("invalid task magazine state tag {tag}"),
        };

        if state
            .compare_exchange(observed, target, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return;
        }
    }
}

fn complete_task_magazine_flush(state: &AtomicU64, sequence: u64) -> bool {
    assert!(
        is_valid_magazine_flush_sequence(sequence),
        "invalid task magazine flush sequence {sequence}"
    );
    let pending_idle = magazine_state_with_sequence(MAGAZINE_STATE_PENDING_IDLE, sequence);
    let pending_active = magazine_state_with_sequence(MAGAZINE_STATE_PENDING_ACTIVE, sequence);
    let frozen = magazine_state_with_sequence(MAGAZINE_STATE_FROZEN, sequence);

    loop {
        let observed = state.load(Ordering::Acquire);
        match observed {
            value if value == pending_idle => {
                if state
                    .compare_exchange(pending_idle, frozen, Ordering::Release, Ordering::Acquire)
                    .is_ok()
                {
                    return true;
                }
            }
            value if value == pending_active => {
                // The interrupted task still owns a mutable magazine borrow.
                // It completes this request after the borrow is dropped.
                return false;
            }
            value if value == frozen => return true,
            MAGAZINE_STATE_IDLE => {
                // A delayed duplicate IPI can arrive after the initiator has
                // ended this sequence.  It must not re-freeze an idle CPU.
                return false;
            }
            MAGAZINE_STATE_ACTIVE => {
                panic!(
                    "task magazine flush sequence {sequence} was completed before request staging"
                );
            }
            value => {
                panic!("task magazine flush sequence {sequence} observed invalid state {value:#x}")
            }
        }
    }
}

/// Freeze an idle local magazine set without staging an IPI request.
///
/// The no-IPI fallback owns only the current CPU, so it has no remote handler
/// that could complete `pending-active`.  Failing this compare-exchange must
/// therefore leave an active local borrow untouched rather than creating a
/// pending state with nobody able to unfreeze it.
#[cfg(any(not(feature = "ipi"), unittest))]
fn try_freeze_idle_task_magazines(state: &AtomicU64, sequence: u64) -> bool {
    assert!(
        is_valid_magazine_flush_sequence(sequence),
        "invalid task magazine flush sequence {sequence}"
    );
    state
        .compare_exchange(
            MAGAZINE_STATE_IDLE,
            magazine_state_with_sequence(MAGAZINE_STATE_FROZEN, sequence),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

fn finish_task_magazine_access(state: &AtomicU64) {
    let pending = match state.compare_exchange(
        MAGAZINE_STATE_ACTIVE,
        MAGAZINE_STATE_IDLE,
        Ordering::Release,
        Ordering::Acquire,
    ) {
        Ok(_) => return,
        Err(observed) => observed,
    };

    let sequence = magazine_state_sequence(pending);
    assert_eq!(
        magazine_state_tag(pending),
        MAGAZINE_STATE_PENDING_ACTIVE,
        "task magazine access left an invalid state {pending:#x}"
    );
    let frozen = magazine_state_with_sequence(MAGAZINE_STATE_FROZEN, sequence);
    state
        .compare_exchange(pending, frozen, Ordering::Release, Ordering::Acquire)
        .unwrap_or_else(|observed| {
            panic!(
                "completing task magazine flush sequence {sequence}: expected pending state \
                 {pending:#x}, found {observed:#x}"
            )
        });
}

impl Default for KmemCacheSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of objects retained for hardirq-only allocation in one class.
const fn irq_reserve_capacity(size_class: SizeClass) -> usize {
    match size_class {
        SizeClass::Bytes8 | SizeClass::Bytes16 => 8,
        SizeClass::Bytes32 => 6,
        SizeClass::Bytes64 => 4,
        SizeClass::Bytes128 => 3,
        SizeClass::Bytes256 => 2,
        SizeClass::Bytes512 => 2,
        SizeClass::Bytes1024 | SizeClass::Bytes2048 => 1,
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::boxed::Box;
    use core::sync::atomic::{AtomicU64, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::{
        KMEM_CACHE_FLUSH_SEQUENCE_MAX, KmemCacheSet, MAGAZINE_STATE_ACTIVE, MAGAZINE_STATE_FROZEN,
        MAGAZINE_STATE_IDLE, SizeClass, complete_task_magazine_flush, finish_task_magazine_access,
        magazine_state_with_sequence, request_task_magazine_flush, try_freeze_idle_task_magazines,
    };

    const PAGE_SIZE: usize = 4096;
    const MAX_SLAB_BYTES: usize = 4 * PAGE_SIZE;

    #[repr(align(16384))]
    struct AlignedSlabStorage([u8; MAX_SLAB_BYTES]);

    fn publish_2048_slab(caches: &KmemCacheSet, storage: &mut AlignedSlabStorage) {
        let base = core::ptr::NonNull::new(storage.0.as_mut_ptr()).unwrap();
        // SAFETY: `storage` is uniquely owned by the test, sufficiently
        // aligned, and remains mapped until the test has detached its span.
        let slab = unsafe { KmemCacheSet::prepare_slab(SizeClass::Bytes2048, base) }.unwrap();
        caches.publish_slab(SizeClass::Bytes2048, slab).unwrap();
    }

    #[def_test]
    fn test_central_cache_publishes_and_reclaims_a_span() {
        let mut storage = Box::new(AlignedSlabStorage([0; MAX_SLAB_BYTES]));
        let base = core::ptr::NonNull::new(storage.0.as_mut_ptr()).unwrap();
        let caches = KmemCacheSet::new();
        publish_2048_slab(&caches, &mut storage);

        let object = caches.try_allocate(SizeClass::Bytes2048).unwrap();
        // SAFETY: `object` was allocated above from the matching class.
        unsafe { caches.deallocate(SizeClass::Bytes2048, object) };

        let backing = caches.take_empty_slab().unwrap();
        assert_eq!(backing.base(), base);
        assert_eq!(
            backing.pages(),
            KmemCacheSet::slab_pages(SizeClass::Bytes2048)
        );
    }

    #[def_test]
    fn test_irq_reserve_is_independent_from_task_cache() {
        let mut storage = Box::new(AlignedSlabStorage([0; MAX_SLAB_BYTES]));
        let caches = KmemCacheSet::new();
        publish_2048_slab(&caches, &mut storage);

        caches.replenish_irq_reserve(SizeClass::Bytes2048);
        assert!(!caches.irq_reserve_needs_refill(SizeClass::Bytes2048));
        let object = caches.try_allocate_irq(SizeClass::Bytes2048).unwrap();
        assert!(caches.irq_reserve_needs_refill(SizeClass::Bytes2048));
        let central_object = caches
            .try_allocate_irq_or_central(SizeClass::Bytes2048)
            .unwrap();
        // SAFETY: `object` was allocated above from the matching class.
        unsafe { caches.deallocate_irq(SizeClass::Bytes2048, object) };
        // SAFETY: `central_object` was allocated above from the matching
        // class through the hardirq reserve/central fallback. The hardirq
        // free path accepts either source and returns it to the reserve or
        // central cache as appropriate.
        unsafe { caches.deallocate_irq(SizeClass::Bytes2048, central_object) };
        assert!(!caches.irq_reserve_needs_refill(SizeClass::Bytes2048));

        let task_object = caches.try_allocate(SizeClass::Bytes2048).unwrap();
        // SAFETY: `task_object` was allocated above from the matching class.
        unsafe { caches.deallocate(SizeClass::Bytes2048, task_object) };
    }

    #[def_test]
    fn test_empty_central_cache_keeps_reserve_donation_requested() {
        let caches = KmemCacheSet::new();
        caches.replenish_irq_reserve(SizeClass::Bytes2048);

        assert!(caches.irq_reserve_needs_refill(SizeClass::Bytes2048));
    }

    #[def_test]
    fn test_idle_magazine_request_is_completed_only_by_the_ipi_side() {
        let state = AtomicU64::new(MAGAZINE_STATE_IDLE);
        let sequence = 1;

        request_task_magazine_flush(&state, sequence);
        assert!(complete_task_magazine_flush(&state, sequence));
        assert_eq!(
            state.load(Ordering::Acquire),
            magazine_state_with_sequence(MAGAZINE_STATE_FROZEN, sequence)
        );

        state.store(MAGAZINE_STATE_IDLE, Ordering::Release);
        assert!(!complete_task_magazine_flush(&state, sequence));
        assert_eq!(state.load(Ordering::Acquire), MAGAZINE_STATE_IDLE);
    }

    #[def_test]
    fn test_active_magazine_request_waits_for_local_borrow_to_finish() {
        let state = AtomicU64::new(MAGAZINE_STATE_ACTIVE);
        let sequence = KMEM_CACHE_FLUSH_SEQUENCE_MAX;

        request_task_magazine_flush(&state, sequence);
        assert!(!complete_task_magazine_flush(&state, sequence));
        finish_task_magazine_access(&state);
        assert_eq!(
            state.load(Ordering::Acquire),
            magazine_state_with_sequence(MAGAZINE_STATE_FROZEN, sequence)
        );
    }

    #[def_test]
    fn test_current_only_freeze_keeps_an_active_borrow_unchanged() {
        let state = AtomicU64::new(MAGAZINE_STATE_ACTIVE);
        assert!(!try_freeze_idle_task_magazines(&state, 1));
        assert_eq!(state.load(Ordering::Acquire), MAGAZINE_STATE_ACTIVE);

        state.store(MAGAZINE_STATE_IDLE, Ordering::Release);
        assert!(try_freeze_idle_task_magazines(&state, 1));
        assert_eq!(
            state.load(Ordering::Acquire),
            magazine_state_with_sequence(MAGAZINE_STATE_FROZEN, 1)
        );
    }
}
