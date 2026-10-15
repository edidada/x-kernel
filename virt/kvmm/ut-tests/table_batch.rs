// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Table construction must retain owned pages until all children are available.

use alloc::boxed::Box;

use kalloc::{FrameRef, FrameRegion, GlobalAllocator, GlobalPage, UsageKind};
use khal::mem::PAGE_SIZE_4K as PAGE;
use unittest::def_test;

struct Fixture {
    allocator: Box<GlobalAllocator>,
    _storage: GlobalPage,
}

impl Fixture {
    fn new() -> Self {
        let storage = GlobalPage::alloc_contiguous(64, PAGE).unwrap();
        let region = FrameRegion::new(storage.start_pa(), storage.start_va(), storage.size());
        let allocator = Box::new(GlobalAllocator::new());
        // SAFETY: the fixture exclusively owns real writable RAM and retains
        // its backing until the private allocator and every local hold retire.
        unsafe { allocator.init_page_allocator_with_regions(&[region]) }.unwrap();
        Self { allocator, _storage: storage }
    }
}

#[def_test]
fn failed_child_batch_returns_every_acquired_page() {
    let fixture = Fixture::new();
    let before = fixture.allocator.available_pages();
    for fail_at in 0..4 {
        let mut attempts = 0;
        let result: Option<[FrameRef<'_>; 4]> = super::try_alloc_table_batch(|| {
            let index = attempts;
            attempts += 1;
            if index == fail_at {
                None
            } else {
                fixture.allocator.alloc_frame(UsageKind::PageTable).ok()
            }
        });
        assert!(result.is_none());
        assert_eq!(attempts, fail_at + 1);
        assert_eq!(fixture.allocator.available_pages(), before,
            "failed construction must return the acquired prefix");
    }
}

#[def_test]
fn complete_child_batch_owns_pages_until_commit_or_drop() {
    let fixture = Fixture::new();
    let before = fixture.allocator.available_pages();
    let batch: [FrameRef<'_>; 4] = super::try_alloc_table_batch(|| {
        fixture.allocator.alloc_frame(UsageKind::PageTable).ok()
    }).unwrap();
    assert_eq!(fixture.allocator.available_pages(), before - 4);
    drop(batch);
    assert_eq!(fixture.allocator.available_pages(), before);
}
