// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Contiguous ownership and base-page retirement tests.

use alloc::boxed::Box;

use alloc_engine::AllocError;
use kspin::NoPreemptIrqSave;
use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddr};
use unittest::def_test;

use crate::{FrameRegion, FrameState, GlobalAllocator, GlobalPage, UsageKind, global_allocator};

struct Fixture {
    allocator: Box<GlobalAllocator>,
    _storage: GlobalPage,
}

impl Fixture {
    fn new() -> Self {
        let storage = GlobalPage::alloc_contiguous(64, PAGE).unwrap();
        let region = FrameRegion::new(
            PhysAddr::from_usize(kaddr_layout::v2p(storage.start_va().as_usize())),
            storage.start_va(),
            storage.size(),
        );
        let allocator = Box::new(GlobalAllocator::new());
        // SAFETY: the exclusive mapped storage outlives this local allocator;
        // field drop order destroys the allocator before returning the storage.
        unsafe { allocator.init_page_allocator_with_regions(&[region]) }.unwrap();
        Self {
            allocator,
            _storage: storage,
        }
    }
}

#[def_test]
fn rounded_run_describes_all_backing_and_retains_one_page_independently() {
    let fixture = Fixture::new();
    let before = fixture.allocator.available_pages();
    let run = fixture
        .allocator
        .alloc_frame_run(3, PAGE, UsageKind::Global)
        .unwrap();
    assert_eq!(run.frame_count(), 4);
    assert_eq!(fixture.allocator.available_pages(), before - 4);
    let first_pfn = run.phys_addr().as_usize() / PAGE;
    for index in 0..4 {
        assert_eq!(
            fixture
                .allocator
                .frame_meta(first_pfn + index)
                .unwrap()
                .state(),
            FrameState::Shared { references: 1 }
        );
    }
    assert!(matches!(
        run.try_clone_frame(4),
        Err(AllocError::InvalidInput)
    ));
    let retained = run.try_clone_frame(3).unwrap();
    let retained_pa = retained.phys_addr();
    assert_eq!(retained_pa, run.phys_addr() + 3 * PAGE);
    drop(run);
    assert_eq!(fixture.allocator.available_pages(), before - 1);
    assert_eq!(retained.usage_kind(), UsageKind::Global);
    assert_eq!(
        fixture.allocator.frame_meta(first_pfn + 3).unwrap().state(),
        FrameState::Shared { references: 1 }
    );
    // Exhaust the private inventory to prove that partial retirement cannot
    // hand the retained page to a new owner.
    let mut replacements = alloc::vec::Vec::new();
    while let Ok(next) = fixture.allocator.alloc_frame(UsageKind::Global) {
        assert_ne!(next.phys_addr(), retained_pa);
        replacements.push(next);
    }
    drop(replacements);
    drop(retained);
    assert_eq!(fixture.allocator.available_pages(), before);
    // Every released base page must coalesce back into the same-order block.
    let next = fixture
        .allocator
        .alloc_frame_run(4, PAGE, UsageKind::Global)
        .unwrap();
    assert_eq!(next.frame_count(), 4);
}

#[def_test]
fn run_rejects_invalid_requests_without_consuming_inventory() {
    let fixture = Fixture::new();
    let before = fixture.allocator.available_pages();
    for (pages, align) in [
        (0, PAGE),
        (usize::MAX, PAGE),
        (usize::MAX / PAGE, PAGE),
        (2, 3),
        (2, 0),
    ] {
        assert!(matches!(
            fixture
                .allocator
                .alloc_frame_run(pages, align, UsageKind::Global),
            Err(AllocError::InvalidInput)
        ));
        assert_eq!(fixture.allocator.available_pages(), before);
    }
    let empty = GlobalAllocator::new();
    assert!(matches!(
        empty.alloc_frame_run(1, PAGE, UsageKind::Global),
        Err(AllocError::NotInitialized)
    ));
}

#[def_test]
fn run_accounts_actual_backing_and_raw_transfer_preserves_counts() {
    let guard = NoPreemptIrqSave::new();
    let allocator = global_allocator();
    let before = crate::current_cpu_allocated_bytes_for_test(&guard);
    let run = allocator
        .alloc_frame_run(5, PAGE, UsageKind::RustHeap)
        .unwrap();
    assert_eq!(run.frame_count(), 8);
    let pfn = run.phys_addr().as_usize() / PAGE;
    assert_eq!(
        crate::current_cpu_allocated_bytes_for_test(&guard) - before,
        8 * PAGE
    );
    let first_peer = run.try_clone_frame(0).unwrap();
    let peer = run.try_clone_frame(7).unwrap();
    let (pa, count) = run.into_raw();
    assert_eq!(
        allocator.frame_meta(pfn + 7).unwrap().state(),
        FrameState::Shared { references: 2 }
    );
    // SAFETY: this exactly consumes the complete run transfer above; all page
    // holds are still live and the fixture never exposes them to hardware.
    let restored = unsafe { allocator.frame_run_from_raw(pa, count) };
    drop(restored);
    assert_eq!(
        allocator.frame_meta(pfn).unwrap().state(),
        FrameState::Shared { references: 1 }
    );
    assert_eq!(
        allocator.frame_meta(pfn + 7).unwrap().state(),
        FrameState::Shared { references: 1 }
    );
    drop(first_peer);
    drop(peer);
}

#[def_test]
fn raw_release_cannot_bypass_any_run_backing_slot() {
    let fixture = Fixture::new();
    let run = fixture
        .allocator
        .alloc_frame_run(3, PAGE, UsageKind::Global)
        .unwrap();
    for index in 0..4 {
        assert_eq!(
            fixture
                .allocator
                .validate_raw_page_release(run.kernel_addr().as_usize() + index * PAGE, 1),
            Err(AllocError::NotAllocated)
        );
    }
    let before = fixture.allocator.available_pages();
    drop(run);
    assert_eq!(fixture.allocator.available_pages(), before + 4);
}

#[def_test]
fn backend_mapping_checks_physical_alignment_independently_of_virtual_alignment() {
    let region = FrameRegion::new(
        PhysAddr::from_usize(PAGE),
        memaddr::VirtAddr::from_usize(2 * PAGE),
        8 * PAGE,
    );
    let mapping = crate::frame_mapping::LinearFrameMapping::new(&[region]).unwrap();
    assert!(mapping.preserves_alignment(PAGE));
    assert!(!mapping.preserves_alignment(2 * PAGE));
}


#[def_test]
fn dma_run_tracks_rounding_slack_and_survives_independent_holders() {
    let fixture = Fixture::new();
    let before = fixture.allocator.available_pages();
    let run = fixture.allocator.alloc_dma_frame_run(3, PAGE, UsageKind::Dma).unwrap();
    assert_eq!(run.frame_count(), 4);
    assert!(run.phys_addr().as_usize() + 4 * PAGE <= 0x1_0000_0000);
    let retained = run.try_clone_frame(3).unwrap();
    drop(run);
    assert_eq!(fixture.allocator.available_pages(), before - 1);
    assert_eq!(retained.usage_kind(), UsageKind::Dma);
    drop(retained);
    assert_eq!(fixture.allocator.available_pages(), before);
}

#[def_test]
fn dma_run_rejects_high_physical_inventory_without_claiming_it() {
    let storage = GlobalPage::alloc_contiguous(64, PAGE).unwrap();
    let allocator = GlobalAllocator::new();
    // The fixture's CPU mapping stays real; only its modeled PA range is high.
    let region = FrameRegion::new(PhysAddr::from_usize(0x1_0000_0000), storage.start_va(), storage.size());
    // SAFETY: exclusively owned mapped storage outlives the local allocator;
    // the modeled PA range is disjoint from all other inventory of this instance.
    unsafe { allocator.init_page_allocator_with_regions(&[region]) }.unwrap();
    let before = allocator.available_pages();
    assert!(matches!(allocator.alloc_dma_frame_run(3, PAGE, UsageKind::Dma), Err(AllocError::NoMemory)));
    assert_eq!(allocator.available_pages(), before);
    let unrestricted = allocator.alloc_frame_run(4, PAGE, UsageKind::Global).unwrap();
    assert!(unrestricted.phys_addr().as_usize() >= 0x1_0000_0000);
    assert_eq!(unrestricted.frame_count(), 4);
}
