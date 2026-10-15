// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Byte heap instance isolation in independent physical inventories.

#![cfg(feature = "slab")]

use alloc::boxed::Box;
use core::alloc::Layout;

use memaddr::PAGE_SIZE_4K as PAGE;
use unittest::def_test;

use crate::{FrameRegion, GlobalAllocator, GlobalPage};

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
