// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Non-concurrent slab-page ownership and free-list management.
//!
//! [`SlabCache`] owns one [`SizeClass`] and records every backing slab page
//! separately.  It does not allocate backing pages or provide synchronization:
//! its caller supplies page-aligned memory, serializes access, and returns
//! slabs yielded by [`SlabCache::take_empty_slab`].

use core::{
    mem,
    ptr::{self, NonNull},
};

use crate::{AllocError, AllocResult, SizeClass};

/// Backing pages detached from an empty slab cache.
///
/// The caller owns these pages after receiving this value and must return
/// them to the page allocator that originally supplied them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "detached slab pages must be returned to their page allocator"]
pub struct SlabBacking {
    base: NonNull<u8>,
    pages: usize,
}

impl SlabBacking {
    /// First byte of the detached page-aligned backing span.
    pub const fn base(self) -> NonNull<u8> {
        self.base
    }

    /// Number of base pages in the detached backing span.
    pub const fn pages(self) -> usize {
        self.pages
    }
}

/// List membership of a slab page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum SlabPageState {
    /// The page has at least one central free object and at least one object
    /// checked out to callers.
    Partial,
    /// The central free list is empty.
    Full,
    /// All objects are on the central free list and the page is reclaimable.
    Empty,
    /// The page is detached and may no longer be accessed through the cache.
    Detached,
}

/// Metadata stored at the first byte of every backing slab page span.
///
/// All fields are protected by the owning [`SlabCache`]'s external
/// synchronization.  Free objects store their intrusive next pointer in the
/// first machine word of their payload.
#[derive(Debug)]
#[repr(C)]
struct SlabPage {
    size_class: SizeClass,
    state: SlabPageState,
    capacity: usize,
    free_count: usize,
    free_head: Option<NonNull<u8>>,
    previous: Option<NonNull<SlabPage>>,
    next: Option<NonNull<SlabPage>>,
}

/// Intrusive list of [`SlabPage`] values in one state.
#[derive(Debug)]
struct SlabList {
    head: Option<NonNull<SlabPage>>,
}

impl SlabList {
    const fn new() -> Self {
        Self { head: None }
    }

    /// Insert `page` at the front of this list.
    ///
    /// # Safety
    ///
    /// `page` must be initialized, detached from every list, and remain valid
    /// while this list retains it.  The caller must serialize all accesses to
    /// the page and this list.
    unsafe fn push_front(&mut self, mut page: NonNull<SlabPage>) {
        // SAFETY: the caller guarantees that `page` is initialized, detached,
        // and exclusively accessible while its list links are updated.
        unsafe {
            let page_ref = page.as_mut();
            debug_assert!(page_ref.previous.is_none());
            debug_assert!(page_ref.next.is_none());

            page_ref.next = self.head;
            if let Some(mut old_head) = self.head {
                old_head.as_mut().previous = Some(page);
            }
            self.head = Some(page);
        }
    }

    /// Remove `page` from this list.
    ///
    /// # Safety
    ///
    /// `page` must be initialized and currently linked into this exact list.
    /// The caller must serialize all accesses to the page and this list.
    unsafe fn remove(&mut self, mut page: NonNull<SlabPage>) {
        // SAFETY: the caller guarantees that `page` belongs to this list and
        // that its links cannot change concurrently.
        unsafe {
            let (previous, next) = {
                let page_ref = page.as_ref();
                (page_ref.previous, page_ref.next)
            };

            if let Some(mut previous) = previous {
                previous.as_mut().next = next;
            } else {
                debug_assert_eq!(self.head, Some(page));
                self.head = next;
            }

            if let Some(mut next) = next {
                next.as_mut().previous = previous;
            }

            let page_ref = page.as_mut();
            page_ref.previous = None;
            page_ref.next = None;
        }
    }
}

/// Non-concurrent cache for one fixed slab [`SizeClass`].
///
/// The cache keeps separate `partial`, `full`, and `empty` page lists.  It
/// never obtains or frees backing memory while mutating these lists: callers
/// use [`add_slab`](Self::add_slab) to publish pages and
/// [`take_empty_slab`](Self::take_empty_slab) to detach reclaimable pages.
///
/// `PAGE_SIZE` must be a non-zero power of two.  A caller must serialize every
/// method call; this type deliberately has no locking policy so it can serve
/// both a standalone byte allocator and a kernel-specific concurrent cache.
#[derive(Debug)]
pub struct SlabCache<const PAGE_SIZE: usize> {
    size_class: SizeClass,
    partial: SlabList,
    full: SlabList,
    empty: SlabList,
    total_blocks: usize,
    free_blocks: usize,
    backing_pages: usize,
}

impl<const PAGE_SIZE: usize> SlabCache<PAGE_SIZE> {
    /// Create an empty cache for `size_class`.
    ///
    /// # Panics
    ///
    /// Panics when `PAGE_SIZE` is zero, is not a power of two, or cannot
    /// represent one power-of-two slab span for `size_class`.
    pub const fn new(size_class: SizeClass) -> Self {
        assert!(PAGE_SIZE != 0);
        assert!(PAGE_SIZE.is_power_of_two());
        assert!(size_class.size() >= mem::align_of::<Option<NonNull<u8>>>());
        assert!(size_class.slab_pages(PAGE_SIZE).is_power_of_two());
        assert!(PAGE_SIZE <= usize::MAX / size_class.slab_pages(PAGE_SIZE));

        Self {
            size_class,
            partial: SlabList::new(),
            full: SlabList::new(),
            empty: SlabList::new(),
            total_blocks: 0,
            free_blocks: 0,
            backing_pages: 0,
        }
    }

    /// Fixed object size class served by this cache.
    pub const fn size_class(&self) -> SizeClass {
        self.size_class
    }

    /// Number of base pages required for one backing slab page span.
    pub const fn slab_pages(&self) -> usize {
        self.size_class.slab_pages(PAGE_SIZE)
    }

    /// Number of bytes in one backing slab page span.
    pub const fn slab_bytes(&self) -> usize {
        self.slab_pages() * PAGE_SIZE
    }

    /// Number of objects managed by the cache, including currently allocated
    /// and centrally free objects.
    pub const fn total_blocks(&self) -> usize {
        self.total_blocks
    }

    /// Number of objects currently available on the central free lists.
    pub const fn free_blocks(&self) -> usize {
        self.free_blocks
    }

    /// Number of backing base pages retained by the cache.
    pub const fn backing_pages(&self) -> usize {
        self.backing_pages
    }

    /// Number of bytes represented by all backing slab spans.
    pub const fn backing_bytes(&self) -> usize {
        self.backing_pages * PAGE_SIZE
    }

    /// Number of bytes currently checked out to callers, rounded to this
    /// cache's object size.
    pub const fn used_bytes(&self) -> usize {
        (self.total_blocks - self.free_blocks) * self.size_class.size()
    }

    /// Allocate one object from this cache.
    ///
    /// Returns `None` when no registered page has a free object.  The caller
    /// should acquire `slab_pages()` pages outside its cache lock, publish them
    /// with [`add_slab`](Self::add_slab), and retry.
    pub fn allocate(&mut self) -> Option<NonNull<u8>> {
        let page = match self.partial.head {
            Some(page) => page,
            None => {
                let page = self.empty.head?;
                self.move_page(page, SlabPageState::Empty, SlabPageState::Partial);
                page
            }
        };

        // SAFETY: `page` belongs to the partial list, so it is initialized,
        // exclusively accessible through `&mut self`, and has a free object.
        let object = unsafe { self.pop_free_object(page) };

        // SAFETY: `page` is initialized and still exclusively owned by this
        // cache.  `pop_free_object` updated its free count.
        let is_full = unsafe { page.as_ref().free_count == 0 };
        if is_full {
            self.move_page(page, SlabPageState::Partial, SlabPageState::Full);
        }
        Some(object)
    }

    /// Publish one externally allocated slab page span to this cache.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::InvalidInput`] when `base` is not aligned to the
    /// size of one slab span or when that span cannot hold both metadata and
    /// at least one object.
    ///
    /// # Safety
    ///
    /// `base` must be the start of an exclusive, writable allocation spanning
    /// exactly `self.slab_pages() * PAGE_SIZE` bytes.  It must remain mapped
    /// and exclusively owned until this cache returns it from
    /// [`take_empty_slab`](Self::take_empty_slab).  The allocation must not
    /// overlap any page already registered with this cache.
    pub unsafe fn add_slab(&mut self, base: NonNull<u8>) -> AllocResult {
        let span_bytes = self.slab_bytes();
        let base_addr = base.as_ptr() as usize;
        if !base_addr.is_multiple_of(span_bytes) {
            return Err(AllocError::InvalidInput);
        }

        let object_offset = self.object_offset()?;
        let capacity = self.object_capacity(object_offset)?;
        let first_object = base_addr
            .checked_add(object_offset)
            .ok_or(AllocError::InvalidInput)?;
        let next_total_blocks = self
            .total_blocks
            .checked_add(capacity)
            .ok_or(AllocError::InvalidInput)?;
        let next_free_blocks = self
            .free_blocks
            .checked_add(capacity)
            .ok_or(AllocError::InvalidInput)?;
        let next_backing_pages = self
            .backing_pages
            .checked_add(self.slab_pages())
            .ok_or(AllocError::InvalidInput)?;
        let mut page = base.cast::<SlabPage>();

        // SAFETY: the caller guarantees a valid, exclusive span at `base`.
        // `object_offset` leaves enough aligned space for `SlabPage`, and the
        // cache initializes every free object before publishing the page.
        unsafe {
            page.as_ptr().write(SlabPage {
                size_class: self.size_class,
                state: SlabPageState::Empty,
                capacity,
                free_count: 0,
                free_head: None,
                previous: None,
                next: None,
            });

            let page_ref = page.as_mut();
            for index in 0..capacity {
                let object_offset = index
                    .checked_mul(self.size_class.size())
                    .expect("slab object offset overflow");
                let object_addr = first_object
                    .checked_add(object_offset)
                    .expect("slab object address overflow");
                let object = NonNull::new_unchecked(object_addr as *mut u8);

                Self::write_next(object, page_ref.free_head);
                page_ref.free_head = Some(object);
                page_ref.free_count += 1;
            }
        }

        self.total_blocks = next_total_blocks;
        self.free_blocks = next_free_blocks;
        self.backing_pages = next_backing_pages;

        // SAFETY: `page` was initialized above and is not linked into any
        // list.  `&mut self` serializes all list mutations.
        unsafe { self.empty.push_front(page) };
        Ok(())
    }

    /// Return an object previously allocated from this cache.
    ///
    /// # Safety
    ///
    /// `object` must be a live object returned by [`allocate`](Self::allocate)
    /// from this same cache and must not have been deallocated before.  The
    /// caller must not access it after this call.
    pub unsafe fn deallocate(&mut self, object: NonNull<u8>) {
        // SAFETY: the caller guarantees that `object` belongs to a live object
        // in this cache, so its size-class-derived slab base contains a valid
        // initialized `SlabPage` header.
        let page = unsafe { self.page_for_object(object) };

        // SAFETY: `page` is valid according to the caller's object ownership
        // contract and all metadata access is serialized by `&mut self`.
        let state = unsafe { page.as_ref().state };
        debug_assert_ne!(state, SlabPageState::Empty);
        debug_assert_ne!(state, SlabPageState::Detached);

        if state == SlabPageState::Full {
            self.move_page(page, SlabPageState::Full, SlabPageState::Partial);
        }

        // SAFETY: `object` is a live object from `page` and is no longer
        // accessible to its caller.  The page metadata is exclusively owned
        // through `&mut self`.
        unsafe { self.push_free_object(page, object) };

        // SAFETY: `page` remains initialized and exclusively accessible; the
        // free count was updated by `push_free_object`.
        let is_empty = unsafe {
            let page_ref = page.as_ref();
            page_ref.free_count == page_ref.capacity
        };
        if is_empty {
            self.move_page(page, SlabPageState::Partial, SlabPageState::Empty);
        }
    }

    /// Detach one completely free slab page span for return to its page
    /// allocator.
    ///
    /// The caller must release the returned pages outside the cache lock.
    pub fn take_empty_slab(&mut self) -> Option<SlabBacking> {
        let mut page = self.empty.head?;

        // SAFETY: `page` is the initialized head of the empty list and `&mut
        // self` serializes its list links and counters.
        unsafe {
            let page_ref = page.as_ref();
            debug_assert_eq!(page_ref.state, SlabPageState::Empty);
            debug_assert_eq!(page_ref.free_count, page_ref.capacity);

            self.empty.remove(page);

            let page_ref = page.as_mut();
            page_ref.state = SlabPageState::Detached;
            self.total_blocks = self
                .total_blocks
                .checked_sub(page_ref.capacity)
                .expect("slab total block underflow");
            self.free_blocks = self
                .free_blocks
                .checked_sub(page_ref.capacity)
                .expect("slab free block underflow");
            self.backing_pages = self
                .backing_pages
                .checked_sub(self.slab_pages())
                .expect("slab backing page underflow");
        }

        Some(SlabBacking {
            base: page.cast(),
            pages: self.slab_pages(),
        })
    }

    fn object_offset(&self) -> AllocResult<usize> {
        let object_size = self.size_class.size();
        let with_alignment_slack = mem::size_of::<SlabPage>()
            .checked_add(object_size - 1)
            .ok_or(AllocError::InvalidInput)?;
        Ok(with_alignment_slack & !(object_size - 1))
    }

    fn object_capacity(&self, object_offset: usize) -> AllocResult<usize> {
        let span_bytes = self.slab_bytes();
        let object_bytes = span_bytes
            .checked_sub(object_offset)
            .ok_or(AllocError::InvalidInput)?;
        let capacity = object_bytes / self.size_class.size();
        if capacity == 0 {
            return Err(AllocError::InvalidInput);
        }
        Ok(capacity)
    }

    fn move_page(&mut self, mut page: NonNull<SlabPage>, from: SlabPageState, to: SlabPageState) {
        // SAFETY: callers only move initialized pages currently linked in the
        // `from` list.  `&mut self` serializes all list and state changes.
        unsafe {
            debug_assert_eq!(page.as_ref().state, from);
            self.list_mut(from).remove(page);
            page.as_mut().state = to;
            self.list_mut(to).push_front(page);
        }
    }

    fn list_mut(&mut self, state: SlabPageState) -> &mut SlabList {
        match state {
            SlabPageState::Partial => &mut self.partial,
            SlabPageState::Full => &mut self.full,
            SlabPageState::Empty => &mut self.empty,
            SlabPageState::Detached => unreachable!("detached slab pages are not list members"),
        }
    }

    /// # Safety
    ///
    /// `page` must be an initialized page in the partial list with at least
    /// one free object.  The caller must serialize access through this cache.
    unsafe fn pop_free_object(&mut self, mut page: NonNull<SlabPage>) -> NonNull<u8> {
        // SAFETY: the caller guarantees exclusive access to an initialized
        // partial page.  Its free-list head was initialized when the page was
        // published and is non-null while `free_count` is non-zero.
        unsafe {
            let page_ref = page.as_mut();
            debug_assert_eq!(page_ref.state, SlabPageState::Partial);
            let object = page_ref
                .free_head
                .take()
                .expect("partial slab page has no free object");
            page_ref.free_head = Self::read_next(object);
            page_ref.free_count = page_ref
                .free_count
                .checked_sub(1)
                .expect("slab page free count underflow");
            self.free_blocks = self
                .free_blocks
                .checked_sub(1)
                .expect("slab free block underflow");
            object
        }
    }

    /// # Safety
    ///
    /// `page` must be an initialized partial page and `object` must be a live
    /// object belonging to it.  The caller must serialize access through this
    /// cache and stop all use of `object` before this call.
    unsafe fn push_free_object(&mut self, mut page: NonNull<SlabPage>, object: NonNull<u8>) {
        // SAFETY: the caller guarantees exclusive page access and that the
        // object is no longer live.  Its class alignment is at least one
        // machine word, so storing the intrusive next pointer is aligned.
        unsafe {
            let page_ref = page.as_mut();
            debug_assert_eq!(page_ref.state, SlabPageState::Partial);
            Self::write_next(object, page_ref.free_head);
            page_ref.free_head = Some(object);
            page_ref.free_count = page_ref
                .free_count
                .checked_add(1)
                .expect("slab page free count overflow");
            self.free_blocks = self
                .free_blocks
                .checked_add(1)
                .expect("slab free block overflow");
        }
    }

    /// # Safety
    ///
    /// `object` must be an object belonging to this cache's size class and to
    /// a currently registered slab page span.
    unsafe fn page_for_object(&self, object: NonNull<u8>) -> NonNull<SlabPage> {
        let span_bytes = self.slab_bytes();
        debug_assert!(span_bytes.is_power_of_two());
        let object_addr = object.as_ptr() as usize;
        let base_addr = object_addr & !(span_bytes - 1);

        // SAFETY: `object` is non-null and the caller guarantees it belongs to
        // a registered span aligned to `span_bytes`, so masking recovers the
        // non-null address of that span's initialized `SlabPage` header.
        let page = unsafe { NonNull::new_unchecked(base_addr as *mut SlabPage) };

        // SAFETY: the caller guarantees that the recovered header belongs to
        // this cache and remains valid while this method reads its metadata.
        unsafe {
            let page_ref = page.as_ref();
            debug_assert_eq!(page_ref.size_class, self.size_class);
            let first_object = base_addr + self.object_offset().expect("valid slab geometry");
            let span_end = base_addr + span_bytes;
            debug_assert!(object_addr >= first_object && object_addr < span_end);
            debug_assert_eq!((object_addr - first_object) % self.size_class.size(), 0);
        }
        page
    }

    /// # Safety
    ///
    /// `object` must be a free object whose first machine word may be read as
    /// an initialized `Option<NonNull<u8>>` link.
    unsafe fn read_next(object: NonNull<u8>) -> Option<NonNull<u8>> {
        // SAFETY: the caller guarantees that the free object's first word was
        // initialized by `write_next` and its size-class alignment satisfies
        // the alignment required by `Option<NonNull<u8>>`.
        unsafe { ptr::read(object.cast::<Option<NonNull<u8>>>().as_ptr()) }
    }

    /// # Safety
    ///
    /// `object` must be a free object whose first machine word may be
    /// overwritten with an intrusive next link.
    unsafe fn write_next(object: NonNull<u8>, next: Option<NonNull<u8>>) {
        // SAFETY: the caller guarantees exclusive access to a free object's
        // first word, and its size-class alignment satisfies the alignment
        // required by `Option<NonNull<u8>>`.
        unsafe { ptr::write(object.cast::<Option<NonNull<u8>>>().as_ptr(), next) };
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::boxed::Box;

    use unittest::def_test;

    use super::{SizeClass, SlabCache};

    const PAGE_SIZE: usize = 4096;
    const MAX_SLAB_BYTES: usize = 4 * PAGE_SIZE;

    #[repr(align(16384))]
    struct AlignedSlabStorage([u8; MAX_SLAB_BYTES]);

    fn slab_storage() -> Box<AlignedSlabStorage> {
        Box::new(AlignedSlabStorage([0; MAX_SLAB_BYTES]))
    }

    #[def_test]
    fn test_cache_needs_a_published_slab() {
        let mut cache = SlabCache::<PAGE_SIZE>::new(SizeClass::Bytes64);
        assert_eq!(cache.allocate(), None);
        assert_eq!(cache.total_blocks(), 0);
        assert_eq!(cache.free_blocks(), 0);
    }

    #[def_test]
    fn test_cache_reuses_a_freed_object() {
        let storage = slab_storage();
        let base = core::ptr::NonNull::new(storage.0.as_ptr() as *mut u8).unwrap();
        let mut cache = SlabCache::<PAGE_SIZE>::new(SizeClass::Bytes128);

        // SAFETY: `storage` is uniquely owned, 16 KiB-aligned, and remains
        // alive until after `cache` is dropped.
        unsafe { cache.add_slab(base).unwrap() };
        let first = cache.allocate().unwrap();
        // SAFETY: `first` was returned by this cache and is not used again
        // before the next allocation.
        unsafe { cache.deallocate(first) };
        let second = cache.allocate().unwrap();
        assert_eq!(first, second);
        // SAFETY: `second` is the current live allocation from this cache.
        unsafe { cache.deallocate(second) };
        assert_eq!(cache.take_empty_slab().unwrap().base(), base);
    }

    #[def_test]
    fn test_cache_reclaims_only_an_empty_slab() {
        let storage = slab_storage();
        let base = core::ptr::NonNull::new(storage.0.as_ptr() as *mut u8).unwrap();
        let mut cache = SlabCache::<PAGE_SIZE>::new(SizeClass::Bytes512);

        // SAFETY: `storage` provides one aligned, exclusive slab span for the
        // selected size class and remains alive for this cache's lifetime.
        unsafe { cache.add_slab(base).unwrap() };
        let object = cache.allocate().unwrap();
        assert_eq!(cache.take_empty_slab(), None);

        // SAFETY: `object` is the sole live allocation from this cache.
        unsafe { cache.deallocate(object) };
        let backing = cache.take_empty_slab().unwrap();
        assert_eq!(backing.base(), base);
        assert_eq!(backing.pages(), SizeClass::Bytes512.slab_pages(PAGE_SIZE));
        assert_eq!(cache.total_blocks(), 0);
        assert_eq!(cache.free_blocks(), 0);
    }

    #[def_test]
    fn test_cache_aligns_every_size_class() {
        let storage = slab_storage();
        let base = core::ptr::NonNull::new(storage.0.as_ptr() as *mut u8).unwrap();

        for size_class in SizeClass::ALL {
            let mut cache = SlabCache::<PAGE_SIZE>::new(size_class);
            // SAFETY: the loop creates and drops one cache at a time, so the
            // shared storage is exclusively registered with the current cache.
            unsafe { cache.add_slab(base).unwrap() };
            let object = cache.allocate().unwrap();
            assert_eq!((object.as_ptr() as usize) % size_class.size(), 0);
            // SAFETY: `object` is the sole live allocation from this cache.
            unsafe { cache.deallocate(object) };
            assert_eq!(cache.take_empty_slab().unwrap().base(), base);
        }
    }
}
