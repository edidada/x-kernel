// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use strum::VariantArray;
use unittest::def_test;

use super::{UsageCounters, UsageKind, UsageTotals};

fn snapshot(counters: &[&UsageCounters]) -> super::Usages {
    let mut totals = UsageTotals::new();
    for counter in counters {
        totals.collect_freed(counter);
    }
    for counter in counters {
        totals.collect_allocated(counter);
    }
    totals.finish()
}

#[def_test]
fn test_usages_alloc_dealloc() {
    let usages = UsageCounters::new();
    usages.alloc(UsageKind::RustHeap, 64);
    assert_eq!(snapshot(&[&usages]).get(UsageKind::RustHeap), 64);
    usages.dealloc(UsageKind::RustHeap, 32);
    assert_eq!(snapshot(&[&usages]).get(UsageKind::RustHeap), 32);
}

#[def_test]
fn test_usage_kind_variants_len() {
    assert!(UsageKind::VARIANTS.len() >= 5);
}

#[def_test]
fn test_usages_independent_kinds() {
    let usages = UsageCounters::new();
    usages.alloc(UsageKind::VirtMem, 10);
    usages.alloc(UsageKind::PageTable, 20);
    let snapshot = snapshot(&[&usages]);
    assert_eq!(snapshot.get(UsageKind::VirtMem), 10);
    assert_eq!(snapshot.get(UsageKind::PageTable), 20);
}

#[def_test]
fn test_usages_allow_cross_cpu_free() {
    let allocating_cpu = UsageCounters::new();
    let freeing_cpu = UsageCounters::new();
    allocating_cpu.alloc(UsageKind::RustHeap, 64);
    freeing_cpu.dealloc(UsageKind::RustHeap, 64);

    assert_eq!(
        snapshot(&[&allocating_cpu, &freeing_cpu]).get(UsageKind::RustHeap),
        0
    );
}

#[def_test]
fn test_remove_memory_requires_all_pages_free() {
    use super::{PAGE_SIZE_4K, global_allocator, global_add_memory, global_remove_memory};
    use super::UsageKind;

    let page = global_allocator()
        .alloc_pages(1, PAGE_SIZE_4K, UsageKind::Global)
        .expect("buddy allocator has a free page");
    assert!(
        global_remove_memory(page, PAGE_SIZE_4K).is_err(),
        "an allocated page must keep its memory block online"
    );

    global_allocator().dealloc_pages(page, 1, UsageKind::Global);
    global_remove_memory(page, PAGE_SIZE_4K)
        .expect("the calling CPU's page cache is flushed before the scan");
    global_add_memory(page, PAGE_SIZE_4K).expect("re-adding a removed region restores its pages");

    let reallocated = global_allocator()
        .alloc_pages_at(page, 1, PAGE_SIZE_4K, UsageKind::Global)
        .expect("the restored page is allocatable again");
    assert_eq!(reallocated, page);
    global_allocator().dealloc_pages(reallocated, 1, UsageKind::Global);
}
