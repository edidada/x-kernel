// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Byte heap ownership and unpublished backing rollback in isolated inventories.

use alloc::boxed::Box;
use core::alloc::Layout;
#[cfg(feature = "slab")]
use core::sync::atomic::Ordering;

#[cfg(feature = "slab")]
use alloc_engine::{BaseAllocator, ByteAllocator};
#[cfg(feature = "slab")]
use kspin::NoPreemptIrqSave;
use memaddr::PAGE_SIZE_4K as PAGE;
use unittest::def_test;

#[cfg(feature = "slab")]
use crate::UsageKind;
use crate::{FrameRegion, FrameState, GlobalAllocator, GlobalPage};

struct Fixture {
    allocator: Box<GlobalAllocator>,
    _storage: GlobalPage,
}

impl Fixture {
    fn new() -> Self {
        let storage = GlobalPage::alloc_contiguous(1024, PAGE).unwrap();
        let region = FrameRegion::new(storage.start_pa(), storage.start_va(), storage.size());
        let allocator = Box::new(GlobalAllocator::new());
        // SAFETY: this fixture exclusively owns real writable RAM, initializes
        // only once and retains storage until its private allocator is destroyed.
        unsafe { allocator.init_page_allocator_with_regions(&[region]) }.unwrap();
        Self {
            allocator,
            _storage: storage,
        }
    }
}

#[cfg(feature = "slab")]
#[def_test]
fn separate_heap_instances_never_exchange_percpu_slab_objects() {
    let left = Fixture::new();
    let right = Fixture::new();
    let layout = Layout::from_size_align(64, 64).unwrap();
    let first = left.allocator.alloc(layout).unwrap();
    left.allocator.dealloc(first, layout);
    let second = right.allocator.alloc(layout).unwrap();
    let second_page = (second.as_ptr() as usize) & !(PAGE - 1);
    let physical = right
        .allocator
        .frame_mapping
        .get()
        .unwrap()
        .physical_address(second_page.into())
        .unwrap();
    assert!(
        right
            .allocator
            .frame_meta(physical.as_usize() / PAGE)
            .is_some(),
        "private heap allocation must come from that instance's storage"
    );
    assert!(
        left.allocator
            .frame_meta(physical.as_usize() / PAGE)
            .is_none()
    );
    // SAFETY: this private allocation has the requested writable layout and no
    // aliases; its fixture retains the entire backing until after deallocation.
    unsafe { core::ptr::write_bytes(second.as_ptr(), 0xa5, layout.size()) };
    right.allocator.dealloc(second, layout);
    let again = left.allocator.alloc(layout).unwrap();
    let left_page = (again.as_ptr() as usize) & !(PAGE - 1);
    let physical = left
        .allocator
        .frame_mapping
        .get()
        .unwrap()
        .physical_address(left_page.into())
        .unwrap();
    assert!(
        left.allocator
            .frame_meta(physical.as_usize() / PAGE)
            .is_some()
    );
    left.allocator.dealloc(again, layout);
}

#[def_test]
fn bootstrap_preserves_existing_pool_and_live_objects() {
    let fixture = Fixture::new();
    let allocator = &fixture.allocator;
    allocator.bootstrap_heap_if_needed().unwrap();
    let layout = Layout::from_size_align(64, 64).unwrap();
    let object = allocator.alloc(layout).unwrap();
    // SAFETY: this fixture exclusively owns the live object for layout.size
    // bytes and publishes no aliases or mappings before the write/read pair.
    unsafe { core::ptr::write_bytes(object.as_ptr(), 0x5a, layout.size()) };
    let before = allocator.available_pages();
    allocator.bootstrap_heap_if_needed().unwrap();
    assert_eq!(allocator.available_pages(), before);
    let second = allocator.alloc(layout).unwrap();
    assert_ne!(
        object, second,
        "initialization must not reset live freelists"
    );
    // SAFETY: both allocations are still owned and disjoint; reinitialization
    // must have preserved the initialized bytes of the first allocation.
    let bytes = unsafe { core::slice::from_raw_parts(object.as_ptr(), layout.size()) };
    assert!(bytes.iter().all(|byte| *byte == 0x5a));
    allocator.dealloc(second, layout);
    allocator.dealloc(object, layout);
}

#[cfg(feature = "slab")]
#[def_test]
fn rejected_heap_extension_returns_inventory_without_charging_a_free() {
    let fixture = Fixture::new();
    let allocator = &fixture.allocator;
    allocator.bootstrap_heap_if_needed().unwrap();
    let mut heap = allocator.balloc.lock();
    for _ in 1..alloc_engine::BuddyPageAllocator::<PAGE>::MAX_REGIONS {
        let address = allocator
            .alloc_pages(crate::MIN_HEAP_SIZE / PAGE, PAGE, UsageKind::RustHeap)
            .unwrap();
        heap.add_region(address, crate::MIN_HEAP_SIZE).unwrap();
    }
    let before = allocator.available_pages();
    let guard = NoPreemptIrqSave::new();
    let counts = || {
        crate::with_current_usage_counters(|counters| {
            let index = UsageKind::RustHeap as usize;
            (
                counters.allocated_bytes[index].load(Ordering::Relaxed),
                counters.freed_bytes[index].load(Ordering::Relaxed),
            )
        })
    };
    let before_counts = counts();
    let before_heap = heap.total_bytes();
    let layout = Layout::from_size_align(64, 64).unwrap();
    assert!(allocator.grow_byte_allocator(&mut heap, layout).is_err());
    assert_eq!(
        allocator.available_pages(),
        before,
        "a rejected region must not strand the acquired backing block"
    );
    assert_eq!(heap.total_bytes(), before_heap);
    assert_eq!(
        counts(),
        before_counts,
        "backing transfers are not caller allocations or deallocations"
    );
    drop(guard);
}

#[def_test]
fn heap_backing_is_owned_even_without_common_page_references() {
    let fixture = Fixture::new();
    let allocator = &fixture.allocator;
    let before = allocator.available_pages();
    allocator.bootstrap_heap_if_needed().unwrap();
    assert!(allocator.available_pages() < before);
    let object = allocator
        .alloc(Layout::from_size_align(64, 64).unwrap())
        .unwrap();
    let page = (object.as_ptr() as usize) & !(PAGE - 1);
    let physical = allocator
        .frame_mapping
        .get()
        .unwrap()
        .physical_address(page.into())
        .unwrap();
    assert_eq!(
        allocator
            .frame_meta(physical.as_usize() / PAGE)
            .unwrap()
            .state(),
        FrameState::Unreferenced
    );
    allocator.dealloc(object, Layout::from_size_align(64, 64).unwrap());
}
