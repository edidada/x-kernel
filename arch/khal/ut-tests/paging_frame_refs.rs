// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kalloc::{FrameState, global_allocator};
use kspin::NoPreemptIrqSave;
use memaddr::PAGE_SIZE_4K;
use page_table::PagingHandler;
use unittest::def_test;

use super::PagingHandlerImpl;

#[def_test]
fn handler_transfers_one_reference_until_table_retirement() {
    // Local IRQ/preemption exclusion also keeps this base-page cache's
    // newly returned page from being reused before the final assertion.
    let _guard = NoPreemptIrqSave::new();
    let pa = PagingHandlerImpl::alloc_frame().unwrap();
    let meta = global_allocator()
        .frame_meta(pa.as_usize() / PAGE_SIZE_4K)
        .unwrap();
    assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
    // The fixture never installs this frame as an active hardware table,
    // so no translation retirement is needed before ending its ownership.
    PagingHandlerImpl::dealloc_frame(pa);
    assert_eq!(meta.state(), FrameState::Unreferenced);
}
#[def_test]
fn owned_replacement_keeps_old_backing_until_tlb_finish() {
    let _guard = NoPreemptIrqSave::new();
    let mut table = super::PageTable::try_new().unwrap();
    let allocator = global_allocator();
    let old = allocator.alloc_frame(kalloc::UsageKind::VirtMem).unwrap();
    let old_meta = allocator.frame_meta(old.pfn()).unwrap();
    let new = allocator
        .alloc_frame_run(1, PAGE_SIZE_4K, kalloc::UsageKind::VirtMem)
        .unwrap();
    let new_meta = allocator
        .frame_meta(new.phys_addr().as_usize() / PAGE_SIZE_4K)
        .unwrap();
    let address = memaddr::VirtAddr::from_usize(0x800000);
    let flags = super::MappingFlags::READ | super::MappingFlags::WRITE;
    let mut edit = table.modify();
    edit.map_owned(address, old, flags).unwrap();
    let snapshot = edit.query_entry(address).unwrap();
    edit.replace_owned_if_same(address, snapshot, new, flags)
        .unwrap();
    assert_eq!(old_meta.state(), FrameState::Referenced { references: 1 });
    let rejected = allocator
        .alloc_frame_run(1, PAGE_SIZE_4K, kalloc::UsageKind::VirtMem)
        .unwrap();
    assert!(matches!(
        edit.replace_owned_if_same(address, snapshot, rejected, flags),
        Err(page_table::PteReplaceError::Changed { .. })
    ));
    assert_eq!(new_meta.state(), FrameState::Referenced { references: 1 });
    edit.finish();
    assert_eq!(old_meta.state(), FrameState::Unreferenced);
    edit.unmap(address).unwrap();
    assert_eq!(new_meta.state(), FrameState::Referenced { references: 1 });
    edit.finish();
    assert_eq!(new_meta.state(), FrameState::Unreferenced);
}

#[def_test]
fn interior_huge_unmap_retires_every_backing_page_after_finish() {
    let _guard = NoPreemptIrqSave::new();
    let size = super::PageSize::Size2M;
    let allocator = global_allocator();
    let owner = allocator
        .alloc_frame_run(
            size as usize / PAGE_SIZE_4K,
            size as usize,
            kalloc::UsageKind::VirtMem,
        )
        .unwrap();
    let first = owner.phys_addr().as_usize() / PAGE_SIZE_4K;
    // Keep a separate observer hold: concurrent tests may reuse pages as
    // soon as they reach buddy, even with this CPU's IRQs disabled.
    let observer = owner.try_clone_run().unwrap();
    let mut table = super::PageTable::try_new().unwrap();
    let address = memaddr::VirtAddr::from_usize(0x1000000);
    let mut edit = table.modify();
    edit.map_owned_sized(address, owner, size, super::MappingFlags::READ)
        .unwrap();
    let (_, _, leaf_size) = edit.unmap(address + 77 * PAGE_SIZE_4K).unwrap();
    assert_eq!(leaf_size, size);
    for index in 0..size as usize / PAGE_SIZE_4K {
        assert_eq!(
            allocator.frame_meta(first + index).unwrap().state(),
            FrameState::Referenced { references: 2 }
        );
    }
    edit.finish();
    for index in 0..size as usize / PAGE_SIZE_4K {
        assert_eq!(
            allocator.frame_meta(first + index).unwrap().state(),
            FrameState::Referenced { references: 1 }
        );
    }
    drop(observer);
}
