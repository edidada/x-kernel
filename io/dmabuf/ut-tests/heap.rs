// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Heap allocation rules that hold without touching the DMA allocator:
//! the length every interface reports and the direction of the cache
//! maintenance a sync request asks for.

use memaddr::PAGE_SIZE_4K;
use unittest::{assert, assert_eq, def_test};

use super::page_aligned_len;

/// `dma_heap_buffer_alloc` rounds the request to a page before the buffer
/// exists, so `lseek(SEEK_END)`, `mmap` and `SYNC` all name one length.
#[def_test]
fn heap_length_rounds_up_to_a_page() {
    assert_eq!(page_aligned_len(1), Some(PAGE_SIZE_4K));
    assert_eq!(page_aligned_len(1000), Some(PAGE_SIZE_4K));
    assert_eq!(page_aligned_len(PAGE_SIZE_4K), Some(PAGE_SIZE_4K));
    assert_eq!(page_aligned_len(PAGE_SIZE_4K + 1), Some(2 * PAGE_SIZE_4K));
    assert_eq!(page_aligned_len(0), Some(0));
    assert_eq!(page_aligned_len(usize::MAX), None);
}

/// The rounding is monotonic, so a larger request never yields a smaller
/// allocation and a request just below a page boundary still fits one page.
#[def_test]
fn heap_rounding_never_shrinks_a_request() {
    for len in [1, 2, PAGE_SIZE_4K - 1, PAGE_SIZE_4K, PAGE_SIZE_4K + 1] {
        let rounded = page_aligned_len(len).expect("a small request must round");
        assert!(rounded >= len);
        assert_eq!(rounded % PAGE_SIZE_4K, 0);
    }
}
