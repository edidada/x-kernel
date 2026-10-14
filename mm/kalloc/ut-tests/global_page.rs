// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GlobalPage's exclusive data views and physical ownership transfer.

use kspin::NoPreemptIrqSave;
use memaddr::PAGE_SIZE_4K as PAGE;
use unittest::def_test;

use crate::{FrameState, GlobalPage, global_allocator};

#[def_test]
fn global_page_owns_rounding_slack_and_raw_roundtrip_keeps_requested_size() {
    let guard = NoPreemptIrqSave::new();
    let mut page = GlobalPage::alloc_contiguous(3, PAGE).unwrap();
    let pa = page.start_pa();
    let pfn = pa.as_usize() / PAGE;
    assert_eq!(page.size(), 3 * PAGE);
    assert_eq!(page.as_slice().len(), 3 * PAGE);
    page.fill(0x5a);
    for index in 0..4 {
        assert_eq!(
            global_allocator().frame_meta(pfn + index).unwrap().state(),
            FrameState::Shared { references: 1 }
        );
    }
    let before = crate::current_cpu_allocated_bytes_for_test(&guard);
    let (raw, count) = page.into_raw();
    assert_eq!(raw, pa);
    assert_eq!(count, 3);
    assert_eq!(
        global_allocator().frame_meta(pfn + 3).unwrap().state(),
        FrameState::Shared { references: 1 }
    );
    // SAFETY: exactly the original GlobalPage transfer is consumed once, with
    // no other owners, payload borrows or hardware users of this allocation.
    let restored = unsafe { GlobalPage::from_raw(raw, count) };
    assert_eq!(restored.start_pa(), pa);
    assert_eq!(restored.size(), 3 * PAGE);
    assert!(restored.as_slice().iter().all(|byte| *byte == 0x5a));
    assert_eq!(crate::current_cpu_allocated_bytes_for_test(&guard), before);
}

#[def_test]
fn global_page_zeroes_requested_bytes_and_supports_physical_alignment() {
    let page = GlobalPage::alloc_zero().unwrap();
    assert!(page.as_slice().iter().all(|byte| *byte == 0));
    let aligned = GlobalPage::alloc_contiguous(5, 8 * PAGE).unwrap();
    assert!(aligned.start_pa().as_usize().is_multiple_of(8 * PAGE));
    assert!(matches!(
        GlobalPage::alloc_contiguous(0, PAGE),
        Err(kerrno::KError::InvalidInput)
    ));
}
