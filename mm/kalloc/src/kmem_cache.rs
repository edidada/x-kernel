// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Slab-only central caches used by the kernel global allocator.
//!
//! Each [`SizeClass`] has an independent [`SlabCache`] and short IRQ-safe lock.
//! Page allocation and slab free-list initialization are deliberately outside
//! those locks; callers publish an already prepared span afterwards.

use core::ptr::NonNull;

use alloc_engine::{
    AllocResult, PreparedSlab, SIZE_CLASS_COUNT, SizeClass, SlabBacking, SlabCache,
};
use kspin::SpinNoIrq;
use memaddr::PAGE_SIZE_4K;

/// Central kernel caches, one for each slab size class.
///
/// The locks protect only `SlabCache` metadata transitions and one-object
/// freelist operations.  Callers must prepare backing spans and return
/// detached pages outside these locks.
pub(crate) struct KmemCacheSet {
    caches: [SpinNoIrq<SlabCache<PAGE_SIZE_4K>>; SIZE_CLASS_COUNT],
}

impl KmemCacheSet {
    /// Create empty central caches for every [`SizeClass`].
    pub const fn new() -> Self {
        Self {
            caches: [
                SpinNoIrq::new(SlabCache::new(SizeClass::Bytes8)),
                SpinNoIrq::new(SlabCache::new(SizeClass::Bytes16)),
                SpinNoIrq::new(SlabCache::new(SizeClass::Bytes32)),
                SpinNoIrq::new(SlabCache::new(SizeClass::Bytes64)),
                SpinNoIrq::new(SlabCache::new(SizeClass::Bytes128)),
                SpinNoIrq::new(SlabCache::new(SizeClass::Bytes256)),
                SpinNoIrq::new(SlabCache::new(SizeClass::Bytes512)),
                SpinNoIrq::new(SlabCache::new(SizeClass::Bytes1024)),
                SpinNoIrq::new(SlabCache::new(SizeClass::Bytes2048)),
            ],
        }
    }

    /// Try to allocate an already available object from `size_class`.
    ///
    /// Returns `None` without allocating backing pages when the central cache
    /// is empty.
    #[inline]
    pub(crate) fn try_allocate(&self, size_class: SizeClass) -> Option<NonNull<u8>> {
        self.cache(size_class).lock().allocate()
    }

    /// Return an object to its class's central cache.
    ///
    /// # Safety
    ///
    /// `object` must be a live allocation from this cache set's matching
    /// `size_class`, and must not have been deallocated before.
    #[inline]
    pub(crate) unsafe fn deallocate(&self, size_class: SizeClass, object: NonNull<u8>) {
        // SAFETY: the caller's object-lifetime contract is forwarded to the
        // class cache while its metadata lock is held.
        unsafe { self.cache(size_class).lock().deallocate(object) };
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
        self.cache(size_class).lock().publish_slab(slab)
    }

    /// Detach one empty span from any size class.
    ///
    /// The caller must return the result to its page allocator outside this
    /// method and outside all central-cache locks.
    pub(crate) fn take_empty_slab(&self) -> Option<SlabBacking> {
        for cache in &self.caches {
            if let Some(backing) = cache.lock().take_empty_slab() {
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

    /// Bytes currently checked out from the central caches.
    pub(crate) fn used_bytes(&self) -> usize {
        self.caches.iter().fold(0usize, |bytes, cache| {
            bytes.saturating_add(cache.lock().used_bytes())
        })
    }

    /// Centrally free object bytes, rounded to their size classes.
    pub(crate) fn available_bytes(&self) -> usize {
        self.caches.iter().fold(0usize, |bytes, cache| {
            let cache = cache.lock();
            bytes.saturating_add(
                cache
                    .free_blocks()
                    .saturating_mul(cache.size_class().size()),
            )
        })
    }

    fn cache(&self, size_class: SizeClass) -> &SpinNoIrq<SlabCache<PAGE_SIZE_4K>> {
        &self.caches[size_class.index()]
    }
}

impl Default for KmemCacheSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::boxed::Box;

    use unittest::def_test;

    use super::{KmemCacheSet, SizeClass};

    const PAGE_SIZE: usize = 4096;
    const MAX_SLAB_BYTES: usize = 4 * PAGE_SIZE;

    #[repr(align(16384))]
    struct AlignedSlabStorage([u8; MAX_SLAB_BYTES]);

    #[def_test]
    fn test_central_cache_publishes_and_reclaims_a_span() {
        let storage = Box::new(AlignedSlabStorage([0; MAX_SLAB_BYTES]));
        let base = core::ptr::NonNull::new(storage.0.as_ptr() as *mut u8).unwrap();
        let caches = KmemCacheSet::new();

        // SAFETY: `storage` is uniquely owned, sufficiently aligned, and
        // remains mapped until after the cache has detached its span.
        let slab = unsafe { KmemCacheSet::prepare_slab(SizeClass::Bytes2048, base) }.unwrap();
        caches.publish_slab(SizeClass::Bytes2048, slab).unwrap();

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
}
