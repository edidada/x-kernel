// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ptr::NonNull;

use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddr};
use unittest::def_test;

use super::{FrameLayout, FrameRegistry};
use crate::{FrameMeta, GlobalPage};

#[def_test]
fn physical_directory_does_not_require_page_content_mappings() {
    let storage = GlobalPage::alloc_contiguous(1, PAGE).unwrap();
    // These physical identities are deliberately unrelated to the mapped
    // descriptor backing. Lookup must never access their page contents.
    let ranges = [
        (PhysAddr::from_usize(512 * PAGE), 64 * PAGE),
        (PhysAddr::from_usize(256 * PAGE), 32 * PAGE),
        (PhysAddr::from_usize(288 * PAGE), 32 * PAGE),
    ];
    let layout = FrameLayout::new(ranges.into_iter()).unwrap();
    assert_eq!(layout.count, 2);
    assert_eq!(layout.described_pages(), 128);
    let descriptors = NonNull::new(storage.start_va().as_mut_ptr().cast()).unwrap();
    // SAFETY: storage is exclusive, page aligned, large enough, and remains
    // allocated until after registry destruction. Only metadata is accessed;
    // the synthetic physical coverage does not require content mappings.
    let registry = unsafe { FrameRegistry::initialize(layout, descriptors, PAGE) }.unwrap();
    assert!(registry.lookup(255).is_none());
    assert!(registry.lookup(256).is_some());
    assert!(registry.lookup(319).is_some());
    assert!(registry.lookup(320).is_none());
    assert!(registry.lookup(511).is_none());
    assert!(registry.lookup(576).is_none());
    let meta = registry.lookup(525).unwrap();
    assert_eq!(
        meta as *const FrameMeta as usize,
        storage.start_va().as_usize() + 77 * core::mem::size_of::<FrameMeta>()
    );
    registry.reserve(575, 1);
    assert!(registry.lookup(575).unwrap().is_permanent());
    assert!(!registry.lookup(574).unwrap().is_permanent());
}
