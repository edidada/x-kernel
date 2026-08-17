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
    sync::atomic::{AtomicBool, Ordering},
};

use alloc_engine::{
    AllocResult, PreparedSlab, SIZE_CLASS_COUNT, SizeClass, SlabBacking, SlabCache,
};
use kspin::SpinNoIrq;
use memaddr::PAGE_SIZE_4K;

use super::magazine::{ObjectBatch, PerCpuMagazineSet};

/// Runtime task magazines associated with the global [`KmemCacheSet`].
///
/// Only `KmemCacheSet` exposes accessors for this storage, keeping the
/// per-CPU policy within the slab-specific cache layer rather than making it
/// a generic global allocator facility.
#[percpu::def_percpu]
static PER_CPU_MAGAZINES: PerCpuMagazineSet = PerCpuMagazineSet::new();

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
    /// # Safety
    ///
    /// The caller must have routed hardirq execution to this cache set's IRQ
    /// reserve and must not invoke this path from NMI context.
    #[inline]
    pub(crate) unsafe fn try_allocate_from_task_magazine(
        &self,
        size_class: SizeClass,
    ) -> Option<NonNull<u8>> {
        PER_CPU_MAGAZINES.with_current(|magazines| magazines.try_allocate(size_class))
    }

    /// Cache a task-context free, returning a full magazine for central drain.
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
    ) -> Option<ObjectBatch> {
        PER_CPU_MAGAZINES.with_current(|magazines| magazines.deallocate(size_class, object))
    }

    /// Install a central refill batch into the current CPU's task magazine.
    ///
    /// Returns objects that could not be installed because both destination
    /// magazines became occupied while the task was preemptible.
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
    ) -> ObjectBatch {
        PER_CPU_MAGAZINES
            .with_current(|magazines| magazines.install_refill(size_class, &mut batch));
        batch
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

    use unittest::{assert, assert_eq, def_test};

    use super::{KmemCacheSet, SizeClass};

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
        // SAFETY: `object` was allocated above from the matching class.
        unsafe { caches.deallocate_irq(SizeClass::Bytes2048, object) };
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
}
