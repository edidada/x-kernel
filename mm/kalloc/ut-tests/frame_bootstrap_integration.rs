// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;

use alloc_engine::AllocError;
use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddr};
use unittest::def_test;

use super::FrameRegion;
use crate::{GlobalAllocator, GlobalPage};

#[def_test]
fn bootstrap_keeps_holes_and_metadata_out_of_inventory() {
    let storage = GlobalPage::alloc_contiguous(256, PAGE).unwrap();
    let base = storage.start_va();
    let physical = storage.start_pa().as_usize();
    let regions = [
        FrameRegion::new(PhysAddr::from_usize(physical), base, 64 * PAGE),
        FrameRegion::new(
            PhysAddr::from_usize(physical + 128 * PAGE),
            base + 128 * PAGE,
            64 * PAGE,
        ),
    ];
    let allocator = Box::new(GlobalAllocator::new());
    // SAFETY: storage owns all these mapped pages, outlives allocator, and
    // no other local allocator can access the fixture during initialization.
    let info = unsafe { allocator.init_page_allocator_with_regions(&regions) }.unwrap();
    let pfn = physical / PAGE;
    assert!(allocator.frame_meta(pfn).is_some());
    assert!(allocator.frame_meta(pfn + 64).is_none());
    assert!(allocator.frame_meta(pfn + 128).is_some());
    assert!(
        allocator
            .frame_meta(info.descriptor_paddr.as_usize() / PAGE)
            .unwrap()
            .is_permanent()
    );
    assert_eq!(info.described_pages, 128);
    assert_eq!(
        info.described_pages,
        info.descriptor_pages + info.buddy_metadata_pages + info.allocatable_pages
    );
    assert_eq!(allocator.available_pages(), info.allocatable_pages);
    // SAFETY: the same fixture is still owned; repeated initialization is
    // rejected before any backing storage is changed.
    let repeated = unsafe { allocator.init_page_allocator_with_regions(&regions) };
    assert_eq!(repeated, Err(AllocError::AlreadyInitialized));
}

#[def_test]
fn bootstrap_reserves_buddy_bitmaps_as_well_as_descriptors() {
    let storage = GlobalPage::alloc_contiguous(512, PAGE).unwrap();
    let base = storage.start_va();
    let physical = storage.start_pa().as_usize();
    let region = FrameRegion::new(PhysAddr::from_usize(physical), base, 512 * PAGE);
    let allocator = Box::new(GlobalAllocator::new());
    // SAFETY: the fixture's exclusive mapped storage outlives allocator,
    // and this is its only initialization.
    let info = unsafe { allocator.init_page_allocator_with_regions(&[region]) }.unwrap();
    assert!(info.buddy_metadata_pages > 0);
    assert!(
        allocator
            .frame_meta(physical / PAGE)
            .unwrap()
            .is_permanent()
    );
    assert_eq!(
        info.described_pages,
        info.descriptor_pages + info.buddy_metadata_pages + info.allocatable_pages
    );
}
