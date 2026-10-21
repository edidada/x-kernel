// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;
use core::sync::atomic::Ordering;

use alloc_engine::AllocError;
use kspin::NoPreemptIrqSave;
use memaddr::PAGE_SIZE_4K as PAGE;
use unittest::def_test;

use super::super::{FrameState, MAX_REFERENCES};
use crate::{FrameRegion, GlobalAllocator, GlobalPage, UsageKind, global_allocator};

struct Fixture {
    // Field order drops the local registry before returning its backing.
    allocator: Box<GlobalAllocator>,
    storage: GlobalPage,
}

impl Fixture {
    fn new() -> Self {
        let storage = GlobalPage::alloc_contiguous(64, PAGE).unwrap();
        let base = storage.start_va();
        let region = FrameRegion::new(storage.start_pa(), base, 64 * PAGE);
        let allocator = Box::new(GlobalAllocator::new());
        // SAFETY: this fixture exclusively owns its writable backing,
        // retains it until allocator teardown, and initializes only once.
        unsafe { allocator.init_page_allocator_with_regions(&[region]) }.unwrap();
        Self { allocator, storage }
    }
}

fn counters(kind: UsageKind) -> (usize, usize) {
    crate::with_current_usage_counters(|counters| {
        let index = kind as usize;
        (
            counters.allocated_bytes[index].load(Ordering::Relaxed),
            counters.freed_bytes[index].load(Ordering::Relaxed),
        )
    })
}

#[def_test]
fn shared_frame_returns_only_after_last_reference() {
    let fixture = Fixture::new();
    let _guard = NoPreemptIrqSave::new();
    let before = counters(UsageKind::Global);
    let owner = fixture.allocator.alloc_frame(UsageKind::Global).unwrap();
    let va = owner.kernel_addr();
    let meta = fixture.allocator.frame_meta(owner.pfn()).unwrap();
    let second = owner.try_clone_ref().unwrap();
    assert_eq!(meta.state(), FrameState::Referenced { references: 2 });
    drop(owner);
    assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
    // A raw caller cannot put the same physical storage into PCP/buddy.
    assert_eq!(
        fixture
            .allocator
            .validate_raw_page_release(va.as_usize(), 1),
        Err(AllocError::NotAllocated)
    );
    assert_eq!(counters(UsageKind::Global), (before.0 + PAGE, before.1));
    drop(second);
    assert_eq!(meta.state(), FrameState::Unreferenced);
    assert_eq!(
        counters(UsageKind::Global),
        (before.0 + PAGE, before.1 + PAGE)
    );
    let reused = fixture.allocator.alloc_frame(UsageKind::Global).unwrap();
    assert_eq!(reused.kernel_addr(), va);
}

#[def_test]
fn frame_zeroing_and_heap_category_accounting() {
    let fixture = Fixture::new();
    let _guard = NoPreemptIrqSave::new();
    let before = counters(UsageKind::RustHeap);
    let owner = fixture.allocator.alloc_frame(UsageKind::RustHeap).unwrap();
    // SAFETY: this is the fixture's only reference, never published to a
    // mapping or another reader; exactly one valid base page is written.
    unsafe { core::ptr::write_bytes(owner.kernel_addr().as_mut_ptr(), 0xa5, PAGE) };
    let va = owner.kernel_addr();
    drop(owner);
    let zeroed = fixture
        .allocator
        .alloc_zeroed_frame(UsageKind::RustHeap)
        .unwrap();
    assert_eq!(zeroed.kernel_addr(), va);
    // SAFETY: this private fixture has not published or cloned the page;
    // the temporary read-only slice is bounded by the live owner.
    let bytes = unsafe { core::slice::from_raw_parts(zeroed.kernel_addr().as_ptr(), PAGE) };
    assert!(bytes.iter().all(|byte| *byte == 0));
    assert_eq!(
        counters(UsageKind::RustHeap),
        (before.0 + 2 * PAGE, before.1 + PAGE)
    );
    drop(zeroed);
    assert_eq!(
        counters(UsageKind::RustHeap),
        (before.0 + 2 * PAGE, before.1 + 2 * PAGE)
    );
}

#[def_test]
fn raw_range_checks_every_slot_and_permanent_backing() {
    let fixture = Fixture::new();
    let owner = fixture.allocator.alloc_frame(UsageKind::Global).unwrap();
    let va = owner.kernel_addr().as_usize();
    assert_eq!(
        fixture.allocator.validate_raw_page_release(va, 1),
        Err(AllocError::NotAllocated)
    );
    if fixture.allocator.frame_meta(owner.pfn() - 1).is_some() {
        assert!(
            fixture
                .allocator
                .validate_raw_page_release(va - PAGE, 2)
                .is_err()
        );
    }
    let registry = fixture.allocator.frames.get().unwrap();
    let backing = fixture
        .allocator
        .frame_meta(fixture.storage.start_pa().as_usize() / PAGE + 63)
        .unwrap();
    assert!(backing.is_permanent());
    assert_eq!(
        registry.validate_raw_release(fixture.storage.start_pa().as_usize() / PAGE + 63, 1),
        Err(AllocError::NotAllocated)
    );
}

#[def_test]
fn reference_overflow_preserves_the_existing_owner() {
    let fixture = Fixture::new();
    let owner = fixture.allocator.alloc_frame(UsageKind::Global).unwrap();
    // This fixture is private and has exactly one holder. Simulate the
    // count boundary, then restore its real count before destruction.
    owner.meta.refs.store(MAX_REFERENCES, Ordering::Relaxed);
    assert!(matches!(owner.try_clone_ref(), Err(AllocError::NoMemory)));
    assert_eq!(
        owner.meta.state(),
        FrameState::Referenced {
            references: MAX_REFERENCES
        }
    );
    owner.meta.refs.store(1, Ordering::Relaxed);
}

#[def_test]
fn raw_reference_roundtrip_does_not_increment_or_release_early() {
    let allocator = global_allocator();
    let owner = allocator.alloc_frame(UsageKind::PageTable).unwrap();
    let pfn = owner.pfn();
    let second = owner.try_clone_ref().unwrap();
    let raw = owner.into_raw();
    assert_eq!(
        allocator.frame_meta(pfn).unwrap().state(),
        FrameState::Referenced { references: 2 }
    );
    // SAFETY: raw owns precisely the reference just transferred by
    // into_raw, from this same static allocator and allocation instance.
    let restored = unsafe { allocator.frame_from_raw(raw) };
    drop(restored);
    assert_eq!(
        allocator.frame_meta(pfn).unwrap().state(),
        FrameState::Referenced { references: 1 }
    );
    drop(second);
}
#[def_test]
fn run_clone_failure_rolls_back_acquired_prefix() {
    let fixture = Fixture::new();
    let owner = fixture
        .allocator
        .alloc_frame_run(4, PAGE, UsageKind::Global)
        .unwrap();
    let first = owner.phys_addr().as_usize() / PAGE;
    let limit = fixture.allocator.frame_meta(first + 2).unwrap();
    // Only this fixture owns the run; restore the artificial count before Drop.
    limit.refs.store(MAX_REFERENCES, Ordering::Relaxed);
    assert!(matches!(owner.try_clone_run(), Err(AllocError::NoMemory)));
    for index in [0, 1, 3] {
        assert_eq!(
            fixture.allocator.frame_meta(first + index).unwrap().state(),
            FrameState::Referenced { references: 1 }
        );
    }
    limit.refs.store(1, Ordering::Relaxed);
    let clone = owner.try_clone_run().unwrap();
    drop(owner);
    for index in 0..4 {
        assert_eq!(
            fixture.allocator.frame_meta(first + index).unwrap().state(),
            FrameState::Referenced { references: 1 }
        );
    }
    drop(clone);
}
