// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use unittest::def_test;

use super::{PAGE_SIZE_4K as PAGE, transition_dma_pages};

#[def_test]
fn dma_transition_reports_failure_after_attempting_every_page() {
    let start = 0x40000000;
    let mut visited = [0usize; 4];
    let mut count = 0;
    let result = transition_dma_pages(start, 4 * PAGE, |address| {
        visited[count] = address;
        count += 1;
        address != start + PAGE
    }, "test-unshare");
    assert_eq!(result, Err(KError::Io));
    assert_eq!(visited, [start, start + PAGE, start + 2 * PAGE, start + 3 * PAGE]);
}

#[def_test]
fn dma_transition_validates_ranges_before_hypercalls() {
    for (start, size) in [(1, PAGE), (0x40000000, PAGE + 1), (usize::MAX - PAGE + 1, PAGE)] {
        let mut called = false;
        let result = transition_dma_pages(start, size, |_| { called = true; true }, "test-share");
        assert_eq!(result, Err(KError::InvalidInput));
        assert!(!called);
    }
    let mut count = 0;
    assert_eq!(transition_dma_pages(0x40000000, 3 * PAGE, |_| { count += 1; true }, "test-share"), Ok(()));
    assert_eq!(count, 3);
}
