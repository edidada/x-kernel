// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Growth with every old region still occupied, including overflow metadata.

use std::alloc::{Layout, alloc_zeroed, dealloc};

use alloc_engine::{AllocError, SlabHeap};

const PAGE: usize = 4096;

struct BackingStorage {
    ptr: *mut u8,
    layout: Layout,
}

impl BackingStorage {
    fn new(pages: usize) -> Self {
        let layout =
            Layout::from_size_align(pages * PAGE, pages.next_power_of_two() * PAGE).unwrap();
        // SAFETY: This nonzero, aligned allocation is exclusively owned by the
        // fixture. Tests retire the nested heap before dropping its storage.
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null());
        Self { ptr, layout }
    }

    fn base(&self) -> usize {
        self.ptr as usize
    }

    fn size(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for BackingStorage {
    fn drop(&mut self) {
        // SAFETY: The heap has been retired, and this fixture uniquely owns
        // the allocation with the exact layout passed to alloc_zeroed.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

#[test]
fn slab_growth_preserves_live_regions_and_reserved_tails() {
    let regions: Vec<_> = (0..80).map(|_| BackingStorage::new(8)).collect();
    // SAFETY: All fixture regions remain mapped and exclusively owned until
    // the heap is explicitly dropped before the fixtures at the end.
    let mut heap = unsafe { SlabHeap::new(regions[0].base(), regions[0].size()) };
    let layout = Layout::from_size_align(PAGE, PAGE).unwrap();
    let mut live = Vec::new();

    for (index, region) in regions.iter().enumerate() {
        if index != 0 {
            // SAFETY: Each fixture is a disjoint, page-aligned live allocation.
            unsafe { heap.add_memory(region.base(), region.size()) }.unwrap();
        }
        let usable_pages = if index < 64 { 8 } else { 7 };
        let mut added = 0;
        while let Ok(addr) = heap.allocate(layout) {
            assert!(addr >= region.base());
            assert!(addr + PAGE <= region.base() + usable_pages * PAGE);
            // SAFETY: This test uniquely owns the newly allocated page. Mark
            // payload independently of allocator headers to detect later damage.
            unsafe { (addr as *mut usize).write(index + 1) };
            live.push((addr, index + 1));
            added += 1;
        }
        assert_eq!(added, usable_pages, "growth region {index} lost capacity");
        // All preceding regions stay allocated while the next is registered.
        for &(addr, payload) in &live {
            // SAFETY: Every recorded allocation is still live and initialized.
            assert_eq!(unsafe { (addr as *const usize).read() }, payload);
        }
        if index >= 64 {
            let tail = region.base() + usable_pages * PAGE;
            // SAFETY: This span is mapped fixture storage; the overlap check
            // must reject it before writing any allocator metadata.
            assert_eq!(
                unsafe { heap.add_memory(tail, PAGE) },
                Err(AllocError::MemoryOverlap)
            );
        }
    }
    assert_eq!(heap.available_bytes(), 0);
    for (addr, _) in live {
        // SAFETY: Each address was allocated once with this exact layout.
        unsafe { heap.deallocate(addr, layout) };
    }
    assert_eq!(heap.available_bytes(), heap.total_bytes());
    drop(heap);
}

#[test]
fn overflow_bitmap_and_descriptor_share_a_checked_reserved_tail() {
    let mut regions: Vec<_> = (0..64).map(|_| BackingStorage::new(2)).collect();
    // 32768 free-head bits fill an entire page, leaving no room for a descriptor.
    regions.push(BackingStorage::new(32768));
    // SAFETY: Fixture storage outlives the heap and is exclusively owned.
    let mut heap = unsafe { SlabHeap::new(regions[0].base(), regions[0].size()) };
    let page_layout = Layout::from_size_align(PAGE, PAGE).unwrap();
    let mut live = Vec::new();
    for region in regions.iter().take(64).skip(1) {
        // SAFETY: Distinct fixture allocations are disjoint and remain mapped.
        unsafe { heap.add_memory(region.base(), region.size()) }.unwrap();
    }
    while let Ok(addr) = heap.allocate(page_layout) {
        live.push(addr);
    }
    assert_eq!(live.len(), 128);
    let extra = &regions[64];
    let old_total = heap.total_bytes();
    // SAFETY: The extra fixture is mapped, exclusively owned and disjoint.
    unsafe { heap.add_memory(extra.base(), extra.size()) }.unwrap();
    assert_eq!(heap.total_bytes() - old_total, 32766 * PAGE);
    let large_layout = Layout::from_size_align(16384 * PAGE, 16384 * PAGE).unwrap();
    let large = heap.allocate(large_layout).unwrap();
    assert_eq!(
        large,
        extra.base(),
        "tail metadata must preserve base alignment"
    );
    while let Ok(addr) = heap.allocate(page_layout) {
        assert!(addr >= extra.base() + large_layout.size());
        assert!(addr + PAGE <= extra.base() + 32766 * PAGE);
        live.push(addr);
    }
    assert_eq!(live.len(), 128 + 32766 - 16384);
    // SAFETY: The large allocation has not been freed and its layout matches.
    unsafe { heap.deallocate(large, large_layout) };
    for addr in live {
        // SAFETY: Each page remains live with this exact layout until this free.
        unsafe { heap.deallocate(addr, page_layout) };
    }
    assert_eq!(heap.available_bytes(), heap.total_bytes());
    drop(heap);
}

#[cfg(feature = "slab")]
#[test]
fn reinitialization_retires_overflow_before_reusing_its_storage() {
    use alloc_engine::{BaseAllocator, ByteAllocator, SlabByteAllocator};

    let regions: Vec<_> = (0..66).map(|_| BackingStorage::new(8)).collect();
    let mut allocator = SlabByteAllocator::new();
    allocator.init_region(regions[0].base(), regions[0].size());
    for region in regions.iter().skip(1) {
        allocator.add_region(region.base(), region.size()).unwrap();
    }
    // Replacement uses storage containing the old allocator's overflow descriptor.
    let replacement = &regions[65];
    allocator.init_region(replacement.base(), replacement.size());
    assert_eq!(allocator.total_bytes(), replacement.size());
    let layout = Layout::from_size_align(PAGE, PAGE).unwrap();
    let mut pages = Vec::new();
    while let Ok(ptr) = allocator.allocate(layout) {
        pages.push(ptr);
    }
    assert_eq!(pages.len(), 8);
    for ptr in pages {
        allocator.deallocate(ptr, layout);
    }
    drop(allocator);
}
