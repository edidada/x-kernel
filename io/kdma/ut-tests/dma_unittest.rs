// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::alloc::Layout;

use alloc_engine::AllocError;
use memaddr::PAGE_SIZE_4K as PAGE;
use unittest::def_test;

use super::{DmaAllocator, layout_pages};

#[def_test]
fn coherent_layout_rounding_rejects_zero_and_preserves_visible_length() {
    assert_eq!(layout_pages(&Layout::from_size_align(0, 1).unwrap()), Err(AllocError::InvalidInput));
    assert_eq!(layout_pages(&Layout::from_size_align(1, 1).unwrap()), Ok(1));
    assert_eq!(layout_pages(&Layout::from_size_align(3 * PAGE - 1, PAGE).unwrap()), Ok(3));
    let mut allocator = DmaAllocator::new();
    // Reject before allocating frames or calling real platform/mapping hooks.
    assert!(matches!(allocator.alloc_coherent_pages(Layout::from_size_align(0, PAGE).unwrap()), Err(AllocError::InvalidInput)));
}
