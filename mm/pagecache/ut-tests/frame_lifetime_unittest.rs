// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File data holds and page-table retirement boundaries.

use kalloc::{FrameState, global_allocator};
use khal::paging::{MappingFlags, PageSize, PageTable, PagingError};
use kspin::NoPreemptIrqSave;
use memaddr::{PAGE_SIZE_4K as PAGE, VirtAddr};
use unittest::def_test;

use crate::Folio;

fn address(page: usize) -> VirtAddr {
    VirtAddr::from_usize(page * PAGE)
}

#[def_test]
fn mapped_folio_outlives_cache_owner_until_flush() {
    // Allocate and return on one guarded CPU so its cache cannot reuse the
    // final page before the diagnostic assertion. The table is never activated.
    let _guard = NoPreemptIrqSave::new();
    let mut folio = Folio::new_zeroed(0).unwrap();
    folio.data()[0] = 0x5a;
    let pa = folio.paddr();
    let meta = global_allocator().frame_meta(pa.as_usize() / PAGE).unwrap();
    let mut table = PageTable::try_new().unwrap();
    let mut modify = table.modify();
    modify
        .map_owned(
            address(1),
            folio.try_clone_frame().unwrap(),
            MappingFlags::READ,
        )
        .unwrap();
    modify.finish();
    assert_eq!(meta.state(), FrameState::Referenced { references: 2 });
    drop(folio);
    assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
    assert_eq!(modify.query(address(1)).unwrap().0, pa);
    // SAFETY: this inactive table still holds the initialized page, and the
    // fixture has no concurrent payload writer or outstanding data borrow.
    let byte = unsafe { khal::mem::p2v(pa).as_ptr().read() };
    assert_eq!(byte, 0x5a);
    modify.unmap(address(1) + 1).unwrap();
    assert_eq!(modify.query(address(1)), Err(PagingError::NotMapped));
    assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
    assert!(modify.finish().had_pending());
    assert_eq!(meta.state(), FrameState::Unreferenced);
}

#[def_test]
fn sparse_invalidation_and_protection_retire_mapping_holds() {
    let _guard = NoPreemptIrqSave::new();
    let first = Folio::new_zeroed(0).unwrap();
    let second = Folio::new_zeroed(1).unwrap();
    let first_meta = global_allocator()
        .frame_meta(first.paddr().as_usize() / PAGE)
        .unwrap();
    let second_meta = global_allocator()
        .frame_meta(second.paddr().as_usize() / PAGE)
        .unwrap();
    let mut table = PageTable::try_new().unwrap();
    let mut modify = table.modify();
    modify
        .map_owned(
            address(1),
            first.try_clone_frame().unwrap(),
            MappingFlags::READ,
        )
        .unwrap();
    modify
        .map_owned(
            address(3),
            second.try_clone_frame().unwrap(),
            MappingFlags::READ,
        )
        .unwrap();
    modify.finish();
    modify
        .protect(address(1) + 1, MappingFlags::empty())
        .unwrap();
    modify
        .unmap_sparse_region(address(1), 3 * PAGE, PageSize::Size4K)
        .unwrap();
    assert_eq!(first_meta.state(), FrameState::Referenced { references: 2 });
    assert_eq!(second_meta.state(), FrameState::Referenced { references: 2 });
    modify.finish();
    assert_eq!(first_meta.state(), FrameState::Referenced { references: 1 });
    assert_eq!(second_meta.state(), FrameState::Referenced { references: 1 });
}

#[def_test]
fn move_mapping_preserves_destination_and_failure_keeps_source() {
    let _guard = NoPreemptIrqSave::new();
    let source = Folio::new_zeroed(0).unwrap();
    let occupied = Folio::new_zeroed(1).unwrap();
    let meta = global_allocator()
        .frame_meta(source.paddr().as_usize() / PAGE)
        .unwrap();
    let mut table = PageTable::try_new().unwrap();
    let mut modify = table.modify();
    modify
        .map_owned(
            address(1),
            source.try_clone_frame().unwrap(),
            MappingFlags::READ,
        )
        .unwrap();
    // Keep external ownership of the raw destination. This exercises failed
    // PTE installation after the new destination hold entered bookkeeping.
    modify
        .map(
            address(2),
            occupied.paddr(),
            PageSize::Size4K,
            MappingFlags::READ,
        )
        .unwrap();
    modify.finish();
    assert_eq!(
        modify.move_mapping(address(1), address(2)),
        Err(PagingError::AlreadyMapped)
    );
    assert_eq!(modify.query(address(1)).unwrap().0, source.paddr());
    assert_eq!(meta.state(), FrameState::Referenced { references: 2 });
    modify.move_mapping(address(1), address(4)).unwrap();
    assert_eq!(modify.query(address(1)), Err(PagingError::NotMapped));
    assert_eq!(modify.query(address(4)).unwrap().0, source.paddr());
    assert_eq!(meta.state(), FrameState::Referenced { references: 3 });
    modify.finish();
    assert_eq!(meta.state(), FrameState::Referenced { references: 2 });
    modify.unmap(address(4)).unwrap();
    modify.finish();
    assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
}

#[def_test]
fn raw_physical_replacement_cannot_detach_an_owned_hold() {
    let _guard = NoPreemptIrqSave::new();
    let first = Folio::new_zeroed(0).unwrap();
    let second = Folio::new_zeroed(1).unwrap();
    let mut table = PageTable::try_new().unwrap();
    let mut modify = table.modify();
    modify
        .map_owned(
            address(1),
            first.try_clone_frame().unwrap(),
            MappingFlags::READ,
        )
        .unwrap();
    modify.finish();
    assert_eq!(
        modify.remap(address(1) + 1, second.paddr(), MappingFlags::READ),
        Err(PagingError::InvalidAddress)
    );
    let snapshot = modify.query_entry(address(1)).unwrap();
    assert!(
        modify
            .replace_if_same(address(1), snapshot, second.paddr(), MappingFlags::READ)
            .is_err()
    );
    assert_eq!(modify.query(address(1)).unwrap().0, first.paddr());
    modify
        .remap(
            address(1),
            first.paddr(),
            MappingFlags::READ | MappingFlags::WRITE,
        )
        .unwrap();
    modify.finish();
}

#[def_test]
fn large_retirement_batch_balances_every_hold() {
    let _guard = NoPreemptIrqSave::new();
    let folios: alloc::vec::Vec<_> = (0..20)
        .map(|index| Folio::new_zeroed(index).unwrap())
        .collect();
    let mut table = PageTable::try_new().unwrap();
    let mut modify = table.modify();
    for (index, folio) in folios.iter().enumerate() {
        modify
            .map_owned(
                address(index + 1),
                folio.try_clone_frame().unwrap(),
                MappingFlags::READ,
            )
            .unwrap();
    }
    modify.finish();
    modify
        .unmap_sparse_region(address(1), 20 * PAGE, PageSize::Size4K)
        .unwrap();
    modify.finish();
    for folio in &folios {
        let meta = global_allocator()
            .frame_meta(folio.paddr().as_usize() / PAGE)
            .unwrap();
        assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
    }
}

#[def_test]
fn dropping_inactive_table_releases_remaining_data_holds() {
    let _guard = NoPreemptIrqSave::new();
    let folio = Folio::new_zeroed(0).unwrap();
    let meta = global_allocator()
        .frame_meta(folio.paddr().as_usize() / PAGE)
        .unwrap();
    let mut table = PageTable::try_new().unwrap();
    table
        .modify()
        .map_owned(
            address(1),
            folio.try_clone_frame().unwrap(),
            MappingFlags::READ,
        )
        .unwrap();
    assert_eq!(meta.state(), FrameState::Referenced { references: 2 });
    // No hardware CPU ever used this root; the table's destruction contract holds.
    drop(table);
    assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
}
