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

#[def_test]
fn empty_permissions_do_not_publish_mapping_holds() {
    let allocator = global_allocator();
    let address = memaddr::VirtAddr::from_usize(0x1800000);
    for size in [super::PageSize::Size4K, super::PageSize::Size2M] {
        let owner = allocator
            .alloc_frame_run(
                size as usize / PAGE_SIZE_4K,
                size as usize,
                kalloc::UsageKind::VirtMem,
            )
            .unwrap();
        let observer = owner.try_clone_run().unwrap();
        let first = owner.phys_addr().as_usize() / PAGE_SIZE_4K;
        let mut table = super::PageTable::try_new().unwrap();
        let mut edit = table.modify();
        edit.map_owned_sized(address, owner, size, super::MappingFlags::empty())
            .unwrap();
        edit.finish();
        assert_eq!(edit.query(address), Err(super::PagingError::NotMapped));
        for index in 0..observer.frame_count() {
            assert_eq!(
                allocator.frame_meta(first + index).unwrap().state(),
                FrameState::Referenced { references: 1 }
            );
        }
        edit.map_owned_sized(
            address,
            observer.try_clone_run().unwrap(),
            size,
            super::MappingFlags::READ,
        )
        .unwrap();
        assert_eq!(
            edit.map_owned_sized(
                address,
                observer.try_clone_run().unwrap(),
                size,
                super::MappingFlags::empty()
            ),
            Err(super::PagingError::AlreadyMapped)
        );
        assert_eq!(edit.query(address).unwrap().0, observer.phys_addr());
        edit.unmap(address).unwrap();
        edit.finish();
        assert_eq!(
            allocator.frame_meta(first).unwrap().state(),
            FrameState::Referenced { references: 1 }
        );
    }
}

#[def_test]
fn empty_remap_and_replacement_retire_mapping_holds() {
    let allocator = global_allocator();
    let address = memaddr::VirtAddr::from_usize(0x1c00000);
    for conditional in [false, true] {
        let owner = allocator.alloc_frame(kalloc::UsageKind::VirtMem).unwrap();
        let observer = owner.try_clone_ref().unwrap();
        let meta = allocator.frame_meta(owner.pfn()).unwrap();
        let mut table = super::PageTable::try_new().unwrap();
        let mut edit = table.modify();
        edit.map_owned(address, owner, super::MappingFlags::READ)
            .unwrap();
        edit.finish();
        if conditional {
            let snapshot = edit.query_entry(address).unwrap();
            edit.replace_if_same(
                address,
                snapshot,
                observer.phys_addr(),
                super::MappingFlags::empty(),
            )
            .unwrap();
        } else {
            edit.remap(address, observer.phys_addr(), super::MappingFlags::empty())
                .unwrap();
        }
        assert_eq!(edit.query(address), Err(super::PagingError::NotMapped));
        assert_eq!(meta.state(), FrameState::Referenced { references: 2 });
        edit.finish();
        assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
        edit.map_owned(
            address,
            observer.try_clone_ref().unwrap(),
            super::MappingFlags::READ,
        )
        .unwrap();
        edit.unmap(address).unwrap();
        edit.finish();
        assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
    }
}

#[def_test]
fn empty_owned_replacement_retires_both_old_and_unused_new_holds() {
    let allocator = global_allocator();
    let old = allocator.alloc_frame(kalloc::UsageKind::VirtMem).unwrap();
    let old_observer = old.try_clone_ref().unwrap();
    let old_meta = allocator.frame_meta(old.pfn()).unwrap();
    let new = allocator
        .alloc_frame_run(1, PAGE_SIZE_4K, kalloc::UsageKind::VirtMem)
        .unwrap();
    let new_observer = new.try_clone_frame(0).unwrap();
    let new_meta = allocator.frame_meta(new_observer.pfn()).unwrap();
    let address = memaddr::VirtAddr::from_usize(0x1e00000);
    let mut table = super::PageTable::try_new().unwrap();
    let mut edit = table.modify();
    edit.map_owned(address, old, super::MappingFlags::READ)
        .unwrap();
    edit.finish();
    let snapshot = edit.query_entry(address).unwrap();
    edit.replace_owned_if_same(address, snapshot, new, super::MappingFlags::empty())
        .unwrap();
    assert_eq!(edit.query(address), Err(super::PagingError::NotMapped));
    assert_eq!(old_meta.state(), FrameState::Referenced { references: 2 });
    assert_eq!(new_meta.state(), FrameState::Referenced { references: 2 });
    edit.finish();
    assert_eq!(old_meta.state(), FrameState::Referenced { references: 1 });
    assert_eq!(new_meta.state(), FrameState::Referenced { references: 1 });
    drop((old_observer, new_observer));
}

#[def_test]
fn huge_conditional_replacement_normalizes_physical_offsets() {
    let allocator = global_allocator();
    let size = super::PageSize::Size2M;
    let owner = allocator
        .alloc_frame_run(
            size as usize / PAGE_SIZE_4K,
            size as usize,
            kalloc::UsageKind::VirtMem,
        )
        .unwrap();
    let observer = owner.try_clone_run().unwrap();
    let first = owner.phys_addr().as_usize() / PAGE_SIZE_4K;
    let address = memaddr::VirtAddr::from_usize(0x2000000);
    let interior = address + 77 * PAGE_SIZE_4K;
    let mut table = super::PageTable::try_new().unwrap();
    let mut edit = table.modify();
    edit.map_owned_sized(address, owner, size, super::MappingFlags::READ)
        .unwrap();
    edit.finish();
    let snapshot = edit.query_entry(interior).unwrap();
    let physical = edit.query(interior).unwrap().0;
    let writable = super::MappingFlags::READ | super::MappingFlags::WRITE;
    edit.replace_if_same(interior, snapshot, physical, writable)
        .unwrap();
    assert_eq!(edit.query(interior).unwrap(), (physical, writable, size));
    let current = edit.query_entry(interior).unwrap();
    assert!(matches!(
        edit.replace_if_same(interior, current, physical + size as usize, writable),
        Err(page_table::PteReplaceError::PageTable(
            super::PagingError::InvalidAddress
        ))
    ));
    assert_eq!(edit.query_entry(interior).unwrap(), current);
    edit.unmap(address).unwrap();
    for index in 0..observer.frame_count() {
        assert_eq!(
            allocator.frame_meta(first + index).unwrap().state(),
            FrameState::Referenced { references: 2 }
        );
    }
    edit.finish();
    for index in 0..observer.frame_count() {
        assert_eq!(
            allocator.frame_meta(first + index).unwrap().state(),
            FrameState::Referenced { references: 1 }
        );
    }
}

#[def_test]
fn owned_unmap_batches_preserve_real_frame_holds_until_completion() {
    let _guard = NoPreemptIrqSave::new();
    let allocator = global_allocator();
    for count in [1, 16, 17, 32, 33, 257] {
        let mut table = super::PageTable::try_new().unwrap();
        let mut frames = alloc::vec::Vec::new();
        let mut edit = table.modify();
        for index in 0..count {
            let owner = allocator.alloc_frame(kalloc::UsageKind::VirtMem).unwrap();
            // An independent observer prevents another CPU from reusing
            // released storage before its mapping-reference assertion.
            frames.push(owner.try_clone_ref().unwrap());
            edit.map_owned(
                memaddr::VirtAddr::from_usize(0x800000 + index * PAGE_SIZE_4K),
                owner,
                super::MappingFlags::READ,
            )
            .unwrap();
        }
        edit.finish();
        for index in 0..count {
            edit.unmap(memaddr::VirtAddr::from_usize(
                0x800000 + index * PAGE_SIZE_4K,
            ))
            .unwrap();
        }
        // Small batches, and the last hold after a pressure-driven drain,
        // must still prevent allocator reuse before the final completion.
        let last = allocator.frame_meta(frames[count - 1].pfn()).unwrap();
        assert_eq!(last.state(), FrameState::Referenced { references: 2 });
        edit.finish();
        for observer in &frames {
            assert_eq!(
                allocator.frame_meta(observer.pfn()).unwrap().state(),
                FrameState::Referenced { references: 1 },
            );
        }
    }
}
