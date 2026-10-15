// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kalloc::{FrameState, UsageKind, global_allocator};
use memaddr::{PAGE_SIZE_4K as PAGE, VirtAddr, VirtAddrRange};
use unittest::def_test;

use super::LinearBackend;
use crate::{FaultContext, backend::BackendOps};
use khal::{paging::{MappingFlags, PageTable, PagingError}, trap::PageFaultFlags};

#[def_test]
fn owned_linear_unmap_retires_mapping_after_backing_owner_ends() {
    let frames = global_allocator().alloc_frame_run(3, PAGE, UsageKind::VirtMem).unwrap();
    let observer = frames.try_clone_frame(0).unwrap();
    let meta = global_allocator().frame_meta(observer.pfn()).unwrap();
    let start = VirtAddr::from_usize(0x200000);
    let backend = LinearBackend::new_owned(start, frames, 3 * PAGE).unwrap();
    let mut table = PageTable::try_new().unwrap();
    let mut edit = table.modify();
    BackendOps::map(&backend, VirtAddrRange::from_start_size(start, 3 * PAGE), MappingFlags::READ, &mut edit).unwrap();
    assert_eq!(meta.state(), FrameState::Referenced { references: 3 });
    drop(backend);
    assert_eq!(edit.query(start).unwrap().0, observer.phys_addr());
    assert_eq!(meta.state(), FrameState::Referenced { references: 2 });
    edit.unmap(start).unwrap();
    assert_eq!(meta.state(), FrameState::Referenced { references: 2 });
    edit.finish();
    assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
}

#[def_test]
fn owned_linear_map_conflict_rolls_back_only_the_new_prefix() {
    let frames = global_allocator().alloc_frame_run(3, PAGE, UsageKind::VirtMem).unwrap();
    let start = VirtAddr::from_usize(0x300000);
    let first = frames.phys_addr().as_usize() / PAGE;
    let backend = LinearBackend::new_owned(start, frames, 3 * PAGE).unwrap();
    let occupied = global_allocator().alloc_frame(UsageKind::VirtMem).unwrap();
    let occupied_pa = occupied.phys_addr();
    let mut table = PageTable::try_new().unwrap();
    let mut edit = table.modify();
    edit.map_owned(start + 2 * PAGE, occupied, MappingFlags::READ).unwrap();
    assert!(BackendOps::map(&backend, VirtAddrRange::from_start_size(start, 3 * PAGE), MappingFlags::READ, &mut edit).is_err());
    assert_eq!(edit.query(start), Err(PagingError::NotMapped));
    assert_eq!(edit.query(start + PAGE), Err(PagingError::NotMapped));
    assert_eq!(edit.query(start + 2 * PAGE).unwrap().0, occupied_pa);
    for index in 0..4 {
        assert_eq!(global_allocator().frame_meta(first + index).unwrap().state(), FrameState::Referenced { references: 1 });
    }
}

#[def_test]
fn owned_linear_rejects_rounding_slack_and_can_refault_after_prot_none() {
    let start = VirtAddr::from_usize(0x400000);
    let frames = global_allocator().alloc_frame_run(3, PAGE, UsageKind::VirtMem).unwrap();
    let pa = frames.phys_addr();
    let backend = LinearBackend::new_owned(start, frames, 3 * PAGE).unwrap();
    let mut table = PageTable::try_new().unwrap();
    let mut edit = table.modify();
    assert!(BackendOps::map(&backend, VirtAddrRange::from_start_size(start, 4 * PAGE), MappingFlags::READ, &mut edit).is_err());
    assert_eq!(edit.query(start), Err(PagingError::NotMapped));
    BackendOps::map(&backend, VirtAddrRange::from_start_size(start, PAGE), MappingFlags::READ, &mut edit).unwrap();
    edit.protect(start, MappingFlags::empty()).unwrap();
    edit.finish();
    BackendOps::handle_fault(&backend, FaultContext::new(start, PageFaultFlags::READ), MappingFlags::READ, &mut edit).unwrap();
    assert_eq!(edit.query(start).unwrap().0, pa);
    assert!(BackendOps::handle_fault(&backend, FaultContext::new(start + 3 * PAGE, PageFaultFlags::READ), MappingFlags::READ, &mut edit).is_err());
    assert!(LinearBackend::new_owned(start, global_allocator().alloc_frame_run(1, PAGE, UsageKind::VirtMem).unwrap(), 2 * PAGE).is_err());
}
