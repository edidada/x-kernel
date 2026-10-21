// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use anon::AnonSharedObject;
use khal::paging::PageSize;
use memaddr::va;
use unittest::def_test;

use super::{SharedBackend, SharedPages};
use crate::{VmBackingKind, backend::BackendOps};

#[def_test]
fn shared_backend_reuses_explicit_shared_owner_identity() {
    let object = AnonSharedObject::new();
    let pages = Arc::new(SharedPages::new(PageSize::Size4K as usize, PageSize::Size4K).unwrap());
    let left = SharedBackend::new(va!(0x1000), pages.clone(), object.clone());
    let right = SharedBackend::new(va!(0x2000), pages, object.clone());
    let VmBackingKind::AnonymousShared { object: left_id } = left.backing_info().kind() else {
        panic!("expected anonymous shared backing");
    };
    let VmBackingKind::AnonymousShared { object: right_id } = right.backing_info().kind() else {
        panic!("expected anonymous shared backing");
    };
    assert_eq!(left_id, object.id());
    assert_eq!(right_id, object.id());
}
#[def_test]
fn shared_backing_survives_backend_drop_and_tlb_retirement() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let start = va!(0x1200000);
    let backend =
        SharedBackend::new_anonymous(start, memaddr::PAGE_SIZE_4K, PageSize::Size4K).unwrap();
    let pa = backend.pages.frames[0].phys_addr();
    let meta = kalloc::global_allocator()
        .frame_meta(pa.as_usize() / memaddr::PAGE_SIZE_4K)
        .unwrap();
    let mut table = khal::paging::PageTable::try_new().unwrap();
    let mut edit = table.modify();
    BackendOps::map(
        &backend,
        memaddr::VirtAddrRange::from_start_size(start, memaddr::PAGE_SIZE_4K),
        khal::paging::MappingFlags::READ,
        &mut edit,
    )
    .unwrap();
    assert_eq!(
        meta.state(),
        kalloc::FrameState::Referenced { references: 2 }
    );
    drop(backend);
    assert_eq!(edit.query(start).unwrap().0, pa);
    assert_eq!(
        meta.state(),
        kalloc::FrameState::Referenced { references: 1 }
    );
    edit.unmap(start).unwrap();
    assert_eq!(
        meta.state(),
        kalloc::FrameState::Referenced { references: 1 }
    );
    edit.finish();
    assert_eq!(meta.state(), kalloc::FrameState::Unreferenced);
}

#[def_test]
fn initially_inaccessible_shared_mappings_release_backing_on_unmap() {
    let page = memaddr::PAGE_SIZE_4K;
    let start = va!(0x2400000);
    let mut space = crate::MmSpace::new_empty_user(start, 16 * page).unwrap();
    for index in 0..4 {
        let address = start + index * page;
        let runtime =
            crate::VmRuntimeRef::new_anon_shared(address, page, PageSize::Size4K).unwrap();
        let crate::VmRuntimeRef::AnonShared(backend) = &runtime else {
            panic!("expected a shared anonymous runtime");
        };
        let observer = backend.pages.frames[0].try_clone_frame(0).unwrap();
        let meta = kalloc::global_allocator()
            .frame_meta(observer.pfn())
            .unwrap();
        space
            .map(
                address,
                page,
                khal::paging::MappingFlags::empty(),
                false,
                runtime,
            )
            .unwrap();
        assert_eq!(
            space.page_table().query(address),
            Err(khal::paging::PagingError::NotMapped)
        );
        space.unmap(address, page).unwrap();
        assert_eq!(
            meta.state(),
            kalloc::FrameState::Referenced { references: 1 },
            "an unmapped PROT_NONE VMA must leave no page-table hold"
        );
    }
}

#[def_test]
fn initially_inaccessible_shared_mapping_refaults_after_read_permission_is_restored() {
    let page = memaddr::PAGE_SIZE_4K;
    let start = va!(0x2500000);
    let mut space = crate::MmSpace::new_empty_user(start, 4 * page).unwrap();
    let runtime = crate::VmRuntimeRef::new_anon_shared(start, page, PageSize::Size4K).unwrap();
    let crate::VmRuntimeRef::AnonShared(backend) = &runtime else {
        panic!("expected a shared anonymous runtime");
    };
    let observer = backend.pages.frames[0].try_clone_frame(0).unwrap();
    let meta = kalloc::global_allocator()
        .frame_meta(observer.pfn())
        .unwrap();
    space
        .map_with_max_flags(
            start,
            page,
            khal::paging::MappingFlags::empty(),
            khal::paging::MappingFlags::READ,
            false,
            runtime,
        )
        .unwrap();
    space
        .protect(start, page, khal::paging::MappingFlags::READ)
        .unwrap();
    space
        .populate_area(start, page, khal::paging::MappingFlags::READ)
        .unwrap();
    assert_eq!(
        space.page_table().query(start).unwrap().0,
        observer.phys_addr()
    );
    space.unmap(start, page).unwrap();
    assert_eq!(
        meta.state(),
        kalloc::FrameState::Referenced { references: 1 }
    );
}
