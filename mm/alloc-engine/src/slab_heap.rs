// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Slab-based byte allocator backed by an internal buddy page allocator.
//!
//! Small allocations are served by one [`SlabCache`] per [`SizeClass`].  Each
//! cache owns the metadata for every backing slab span, so completely empty
//! spans can be detached and returned to the internal buddy allocator.
//! Requests outside the slab size classes are allocated directly from buddy.

use core::{alloc::Layout, ptr::NonNull};

use crate::{AllocError, BuddyAllocator, SIZE_CLASS_COUNT, SizeClass, slab_cache::SlabCache};

/// Internal base page size used by this byte allocator.
const PAGE_SIZE: usize = 4096;

/// Size-class slab byte allocator.
///
/// `SlabHeap` manages a fixed memory region with an internal buddy allocator.
/// It owns one [`SlabCache`] for each [`SizeClass`]; backing spans are allocated
/// from and returned to that buddy allocator only at cache refill and shrink
/// boundaries.
pub struct SlabHeap {
    slabs: [SlabCache<PAGE_SIZE>; SIZE_CLASS_COUNT],
    /// Internal buddy for heap regions and requests outside slab classes.
    buddy: BuddyAllocator<PAGE_SIZE>,
}

impl SlabHeap {
    /// Creates a new heap backed by memory at `[start, start + size)`.
    ///
    /// # Safety
    ///
    /// The caller must ensure the memory range is valid, not used for anything
    /// else, and remains accessible for the lifetime of this heap.
    pub unsafe fn new(start: usize, size: usize) -> Self {
        let mut heap = Self {
            slabs: Self::new_slab_caches(),
            buddy: BuddyAllocator::new(),
        };
        heap.buddy.init_region(start, size).unwrap_or_else(|error| {
            panic!("SlabHeap::new init_region({start:#x}, {size:#x}) failed: {error:?}",)
        });
        heap
    }

    /// Adds more memory to the heap.
    ///
    /// # Safety
    ///
    /// The caller must ensure the memory range is valid, does not overlap an
    /// existing managed range, and remains accessible for this heap's lifetime.
    pub unsafe fn add_memory(&mut self, start: usize, size: usize) {
        self.buddy.add_region(start, size).unwrap_or_else(|error| {
            panic!("SlabHeap::add_memory add_region({start:#x}, {size:#x}) failed: {error:?}",)
        });
    }

    /// Allocates memory with `layout`.
    ///
    /// Small layouts use the smallest matching [`SizeClass`].  Other layouts
    /// are allocated directly from the internal buddy allocator.
    ///
    /// # Errors
    ///
    /// Returns an allocation error when the backing buddy allocator cannot
    /// satisfy a refill or direct allocation request.
    pub fn allocate(&mut self, layout: Layout) -> Result<usize, AllocError> {
        match SizeClass::from_layout(layout) {
            Some(size_class) => self.allocate_slab(size_class),
            None => {
                let page_count = layout.size().div_ceil(PAGE_SIZE);
                let alignment = layout.align().max(PAGE_SIZE);
                self.allocate_pages_with_reclaim(page_count, alignment)
            }
        }
    }

    /// Frees a block previously allocated by [`allocate`](Self::allocate).
    ///
    /// # Safety
    ///
    /// `ptr` must have been returned by a previous call to
    /// [`allocate`](Self::allocate) with the same `layout`, must not have been
    /// deallocated already, and must not be accessed after this call.
    pub unsafe fn deallocate(&mut self, ptr: usize, layout: Layout) {
        match SizeClass::from_layout(layout) {
            Some(size_class) => {
                // SAFETY: the caller guarantees `ptr` came from this heap, so
                // a slab allocation is necessarily non-null.
                let object = unsafe { NonNull::new_unchecked(ptr as *mut u8) };
                // SAFETY: the caller's allocation/one-time-free contract is
                // exactly the `SlabCache::deallocate` contract for this class.
                unsafe { self.slab_cache_mut(size_class).deallocate(object) };
            }
            None => {
                let page_count = layout.size().div_ceil(PAGE_SIZE);
                self.buddy.deallocate_pages(ptr, page_count);
            }
        }
    }

    /// Detach all completely free slab spans and return them to buddy.
    ///
    /// The return value is the number of base pages released.  Spans with one
    /// or more live objects remain cached and are not returned.
    #[must_use = "the released page count is useful to reclaim and memory-pressure callers"]
    pub fn shrink(&mut self) -> usize {
        let mut released_pages = 0usize;

        for index in 0..SIZE_CLASS_COUNT {
            while let Some(backing) = self.slabs[index].take_empty_slab() {
                self.buddy
                    .deallocate_pages(backing.base().as_ptr() as usize, backing.pages());
                released_pages = released_pages.saturating_add(backing.pages());
            }
        }

        released_pages
    }

    /// Total managed heap size in bytes.
    pub fn total_bytes(&self) -> usize {
        self.buddy.managed_bytes()
    }

    /// Currently allocated bytes.
    ///
    /// Small allocations are reported in their size-class granularity.  Large
    /// allocations retain buddy's page granularity.
    pub fn used_bytes(&self) -> usize {
        let direct_buddy_bytes = self
            .buddy
            .allocated_bytes()
            .saturating_sub(self.slab_backing_bytes());
        self.slab_used_bytes().saturating_add(direct_buddy_bytes)
    }

    /// Available bytes according to the heap's allocation accounting.
    pub fn available_bytes(&self) -> usize {
        self.total_bytes().saturating_sub(self.used_bytes())
    }

    fn new_slab_caches() -> [SlabCache<PAGE_SIZE>; SIZE_CLASS_COUNT] {
        [
            SlabCache::new(SizeClass::Bytes8),
            SlabCache::new(SizeClass::Bytes16),
            SlabCache::new(SizeClass::Bytes32),
            SlabCache::new(SizeClass::Bytes64),
            SlabCache::new(SizeClass::Bytes128),
            SlabCache::new(SizeClass::Bytes256),
            SlabCache::new(SizeClass::Bytes512),
            SlabCache::new(SizeClass::Bytes1024),
            SlabCache::new(SizeClass::Bytes2048),
        ]
    }

    fn allocate_slab(&mut self, size_class: SizeClass) -> Result<usize, AllocError> {
        if let Some(object) = self.slab_cache_mut(size_class).allocate() {
            return Ok(object.as_ptr() as usize);
        }

        let (page_count, span_alignment) = {
            let cache = self.slab_cache(size_class);
            (cache.slab_pages(), cache.slab_bytes())
        };
        let base = self.allocate_pages_with_reclaim(page_count, span_alignment)?;
        // SAFETY: a successful buddy allocation is a non-null, mapped span
        // with exactly `page_count` base pages and `span_alignment` alignment.
        let backing = unsafe { NonNull::new_unchecked(base as *mut u8) };

        let publish_result = {
            let cache = self.slab_cache_mut(size_class);
            // SAFETY: `backing` was allocated above with the cache's page
            // count and span alignment.  It is exclusively owned until a
            // later `take_empty_slab` detaches it.
            unsafe { cache.add_slab(backing) }
        };
        if let Err(error) = publish_result {
            self.buddy.deallocate_pages(base, page_count);
            return Err(error);
        }

        self.slab_cache_mut(size_class)
            .allocate()
            .map(|object| object.as_ptr() as usize)
            .ok_or(AllocError::NoMemory)
    }

    fn slab_cache(&self, size_class: SizeClass) -> &SlabCache<PAGE_SIZE> {
        &self.slabs[size_class.index()]
    }

    fn slab_cache_mut(&mut self, size_class: SizeClass) -> &mut SlabCache<PAGE_SIZE> {
        &mut self.slabs[size_class.index()]
    }

    /// Allocate pages after making one bounded attempt to release empty slabs.
    ///
    /// Empty spans are useful as a warm cache, so the normal allocation and
    /// deallocation paths retain them.  Once the internal buddy cannot satisfy
    /// a request, retaining those spans would report a false OOM even though
    /// the heap still owns fully free memory.
    fn allocate_pages_with_reclaim(
        &mut self,
        page_count: usize,
        alignment: usize,
    ) -> Result<usize, AllocError> {
        match self.buddy.allocate_pages(page_count, alignment) {
            Err(AllocError::NoMemory) if self.shrink() != 0 => {
                self.buddy.allocate_pages(page_count, alignment)
            }
            result => result,
        }
    }

    fn slab_backing_bytes(&self) -> usize {
        self.slabs.iter().fold(0, |bytes, cache| {
            bytes.saturating_add(cache.backing_bytes())
        })
    }

    fn slab_used_bytes(&self) -> usize {
        self.slabs
            .iter()
            .fold(0, |bytes, cache| bytes.saturating_add(cache.used_bytes()))
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use core::alloc::Layout;

    use unittest::def_test;

    use super::{PAGE_SIZE, SizeClass, SlabHeap};

    fn make_heap() -> SlabHeap {
        let memory = alloc::vec![0u8; 128 * PAGE_SIZE];
        let base = memory.as_ptr() as usize;
        let size = memory.len();
        core::mem::forget(memory);
        // SAFETY: the Vec's backing memory is leaked and remains valid.
        unsafe { SlabHeap::new(base, size) }
    }

    #[def_test]
    fn test_alloc_dealloc_small() {
        let mut heap = make_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let first = heap.allocate(layout).unwrap();
        let second = heap.allocate(layout).unwrap();
        assert_ne!(first, second);
        // SAFETY: `first` was allocated above with this layout.
        unsafe { heap.deallocate(first, layout) };
        // SAFETY: `second` was allocated above with this layout.
        unsafe { heap.deallocate(second, layout) };
    }

    #[def_test]
    fn test_reuse_freed_block() {
        let mut heap = make_heap();
        let layout = Layout::from_size_align(128, 8).unwrap();
        let first = heap.allocate(layout).unwrap();
        // SAFETY: `first` was allocated above with this layout.
        unsafe { heap.deallocate(first, layout) };
        let second = heap.allocate(layout).unwrap();
        assert_eq!(first, second);
        // SAFETY: `second` was allocated above with this layout.
        unsafe { heap.deallocate(second, layout) };
    }

    #[def_test]
    fn test_large_alloc() {
        let mut heap = make_heap();
        let layout = Layout::from_size_align(8192, 8).unwrap();
        let address = heap.allocate(layout).unwrap();
        assert!(address.is_multiple_of(PAGE_SIZE));
        // SAFETY: `address` was allocated above with this layout.
        unsafe { heap.deallocate(address, layout) };
    }

    #[def_test]
    fn test_many_allocs() {
        let mut heap = make_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let mut addresses = alloc::vec::Vec::new();
        for _ in 0..100 {
            addresses.push(heap.allocate(layout).unwrap());
        }
        for address in addresses {
            // SAFETY: every address was allocated above with this layout.
            unsafe { heap.deallocate(address, layout) };
        }
    }

    #[def_test]
    fn test_mixed_sizes() {
        let mut heap = make_heap();
        let layout_64 = Layout::from_size_align(64, 8).unwrap();
        let layout_256 = Layout::from_size_align(256, 8).unwrap();
        let layout_1024 = Layout::from_size_align(1024, 8).unwrap();

        let address_64 = heap.allocate(layout_64).unwrap();
        let address_256 = heap.allocate(layout_256).unwrap();
        let address_1024 = heap.allocate(layout_1024).unwrap();

        // SAFETY: every address was allocated above with its matching layout.
        unsafe { heap.deallocate(address_256, layout_256) };
        // SAFETY: every address was allocated above with its matching layout.
        unsafe { heap.deallocate(address_64, layout_64) };
        // SAFETY: every address was allocated above with its matching layout.
        unsafe { heap.deallocate(address_1024, layout_1024) };

        assert!(heap.allocate(layout_1024).is_ok());
    }

    #[def_test]
    fn test_shrink_releases_only_empty_slab_pages() {
        let mut heap = make_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let address = heap.allocate(layout).unwrap();

        assert_eq!(heap.used_bytes(), SizeClass::Bytes64.size());
        assert_eq!(heap.shrink(), 0);

        // SAFETY: `address` is the only allocation from this heap.
        unsafe { heap.deallocate(address, layout) };
        assert_eq!(heap.shrink(), SizeClass::Bytes64.slab_pages(PAGE_SIZE),);
        assert_eq!(heap.used_bytes(), 0);
        assert_eq!(heap.available_bytes(), heap.total_bytes());
    }

    #[def_test]
    fn test_large_slab_span_is_aligned_and_reclaimable() {
        let mut heap = make_heap();
        let layout = Layout::from_size_align(2048, 2048).unwrap();
        let address = heap.allocate(layout).unwrap();
        assert!(address.is_multiple_of(2048));

        // SAFETY: `address` was allocated above with this layout.
        unsafe { heap.deallocate(address, layout) };
        assert_eq!(heap.shrink(), SizeClass::Bytes2048.slab_pages(PAGE_SIZE),);
    }

    #[def_test]
    fn test_page_pressure_reclaims_empty_slab_spans() {
        let mut heap = make_heap();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let address = heap.allocate(layout).unwrap();

        // SAFETY: `address` is the only live allocation from this heap.
        unsafe { heap.deallocate(address, layout) };

        let mut allocated_pages = alloc::vec::Vec::new();
        while let Ok(address) = heap.buddy.allocate_pages(1, PAGE_SIZE) {
            allocated_pages.push(address);
        }

        let reclaimed_page = heap.allocate_pages_with_reclaim(1, PAGE_SIZE).unwrap();
        assert_eq!(heap.shrink(), 0);

        heap.buddy.deallocate_pages(reclaimed_page, 1);
        for address in allocated_pages {
            heap.buddy.deallocate_pages(address, 1);
        }
    }

    #[def_test]
    fn test_stats() {
        let heap = make_heap();
        assert!(heap.total_bytes() > 0);
        assert_eq!(heap.available_bytes(), heap.total_bytes());
    }
}
