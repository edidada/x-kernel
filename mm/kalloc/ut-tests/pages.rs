// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{alloc::Layout, sync::atomic::Ordering};

use kspin::NoPreemptIrqSave;
use memaddr::PAGE_SIZE_4K;
use unittest::def_test;

use crate::{UsageKind, global_allocator, page_allocator};

fn counters(kind: UsageKind) -> (usize, usize) {
    crate::with_current_usage_counters(|counter| (
        counter.allocated_bytes[kind as usize].load(Ordering::Relaxed),
        counter.freed_bytes[kind as usize].load(Ordering::Relaxed),
    ))
}

#[def_test]
fn page_service_delivers_physical_pages_and_reclaims_the_same_pcp_slot() {
    let _guard = NoPreemptIrqSave::new();
    let pages = page_allocator();
    let before = counters(UsageKind::PageTable);
    let pa = pages.alloc_pages(1, PAGE_SIZE_4K, UsageKind::PageTable).unwrap();
    assert!(pa.as_usize().is_multiple_of(PAGE_SIZE_4K));
    let va = kaddr_layout::p2v(pa.as_usize());
    // SAFETY: this test owns the newly allocated page and its permanent writable
    // direct mapping. No content borrow survives the matching physical release.
    unsafe { (va as *mut usize).write(0x938); }
    // SAFETY: the initialized word remains exclusively owned by this test.
    assert_eq!(unsafe { (va as *const usize).read() }, 0x938);
    assert_eq!(counters(UsageKind::PageTable), (before.0 + PAGE_SIZE_4K, before.1));
    pages.dealloc_pages(pa, 1, UsageKind::PageTable);
    // IRQ/preemption exclusion retains the just-freed page in this CPU's PCP.
    let reused = pages.alloc_pages(1, PAGE_SIZE_4K, UsageKind::PageTable).unwrap();
    assert_eq!(reused, pa);
    pages.dealloc_pages(reused, 1, UsageKind::PageTable);
    assert_eq!(counters(UsageKind::PageTable), (before.0 + 2 * PAGE_SIZE_4K, before.1 + 2 * PAGE_SIZE_4K));
}

#[def_test]
fn heap_backing_and_page_objects_have_distinct_accounting() {
    let _guard = NoPreemptIrqSave::new();
    let pages = page_allocator();
    let before = counters(UsageKind::RustHeap);
    // Eight pages bypass PCP and exercise the shared buddy inventory. Heap
    // growth obtains backing here without charging the objects it will contain.
    let backing = pages.alloc_from_inventory(8, PAGE_SIZE_4K).unwrap();
    assert_eq!(counters(UsageKind::RustHeap), before);
    pages.release_to_inventory(backing, 8);
    assert_eq!(counters(UsageKind::RustHeap), before);

    let page = pages.alloc_pages(1, PAGE_SIZE_4K, UsageKind::RustHeap).unwrap();
    assert_eq!(counters(UsageKind::RustHeap), (before.0 + PAGE_SIZE_4K, before.1));
    pages.dealloc_pages(page, 1, UsageKind::RustHeap);
    assert_eq!(counters(UsageKind::RustHeap), (before.0 + PAGE_SIZE_4K, before.1 + PAGE_SIZE_4K));
}

#[def_test]
fn heap_small_and_large_requests_charge_once_through_different_backends() {
    let _guard = NoPreemptIrqSave::new();
    let heap = global_allocator();
    let before = counters(UsageKind::RustHeap);
    let small = Layout::from_size_align(64, 8).unwrap();
    let object = heap.alloc(small).unwrap();
    assert_eq!(counters(UsageKind::RustHeap), (before.0 + small.size(), before.1));
    heap.dealloc(object, small);

    // The heap's large path goes directly to the physical page service.
    let large = Layout::from_size_align(2 * 1024 * 1024, PAGE_SIZE_4K).unwrap();
    let object = heap.alloc(large).unwrap();
    assert_eq!(counters(UsageKind::RustHeap), (before.0 + small.size() + large.size(), before.1 + small.size()));
    heap.dealloc(object, large);
    assert_eq!(counters(UsageKind::RustHeap), (before.0 + small.size() + large.size(), before.1 + small.size() + large.size()));
}
