// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[expect(
    clippy::unwrap_used,
    reason = "CG-021-no-panic-on-reachable-failure: test scenario: allocate live backing"
)]
fn new_frame() -> kalloc::FrameRun<'static> {
    kalloc::global_allocator()
        .alloc_frame_run(1, khal::mem::PAGE_SIZE_4K, kalloc::UsageKind::VirtMem)
        .unwrap()
}

use khal::paging::PageSize;
use unittest::def_test;
use vmobj::{MappingViewKind, MappingViewSpec};

use super::{AnonPrivateObject, AnonPrivatePageCommitError};

#[def_test]
fn anon_private_fork_child_keeps_lineage_and_gets_new_identity() {
    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();

    assert_eq!(parent.lineage(), child.lineage());
    assert_ne!(parent.id(), child.id());
    assert_ne!(parent.anon_id(), child.anon_id());
}

#[def_test]
fn anon_private_registers_views_and_emits_object_hits() {
    let object = AnonPrivateObject::new_root();
    let _view = object.register_view(MappingViewSpec {
        mm_id: 9,
        vma_start: 0x9000,
        vma_len: 0x3000,
        object_start: 0x0,
        object_len: 0x3000,
        kind: MappingViewKind::Private,
        notifier: None,
    });

    let work = object
        .invalidate_range(0x1000, 0x1000)
        .expect("private anon object hit must be reported");
    assert_eq!(work.hits().len(), 1);
    let hit = &work.hits()[0];
    assert_eq!(hit.view().mm_id(), 9);
    assert_eq!(hit.vma_start(), 0xa000);
    assert_eq!(hit.object_start(), 0x1000);
    assert_eq!(hit.object_len(), 0x1000);
}

#[def_test]
fn anon_private_page_state_fork_and_discard_work() {
    let frame_0 = new_frame();
    let pa_0 = frame_0.phys_addr();

    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();
    let handle = parent
        .install_page(0x1000, frame_0, PageSize::Size4K)
        .expect("install private page");
    assert!(handle.is_exclusive());

    let prepared = parent
        .prepare_fork_child_pages(0x1000, 0x1000, &child)
        .expect("prepare fork child pages");
    assert_eq!(prepared.pages().len(), 1);
    assert_eq!(prepared.pages()[0].object_start(), 0x1000);
    assert!(!handle.is_exclusive());
    prepared.commit().expect("commit fork child pages");
    assert_eq!(child.page_at(0x1000).expect("child page").phys_addr(), pa_0);

    let released = parent.detach_range(0x1000, 0x1000).finalize_release();
    assert!(released.is_empty());
    let released = child.detach_range(0x1000, 0x1000).finalize_release();
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].phys_addr(), pa_0);
}

#[def_test]
fn anon_private_first_touch_prepare_commits_page_slot() {
    let frame_0 = new_frame();
    let pa_0 = frame_0.phys_addr();

    let object = AnonPrivateObject::new_root();
    let prepared = object
        .prepare_first_touch_page(0x1000)
        .expect("prepare first-touch page");

    assert!(
        object.page_at(0x1000).is_none(),
        "prepared first-touch page must not publish object state"
    );

    let handle = prepared
        .commit(frame_0, PageSize::Size4K)
        .expect("commit first-touch page");
    assert_eq!(handle.phys_addr(), pa_0);
    assert_eq!(
        object
            .page_at(0x1000)
            .expect("committed page slot")
            .phys_addr(),
        pa_0
    );
}

#[def_test]
fn anon_private_first_touch_commit_revalidates_empty_slot() {
    let frame_0 = new_frame();
    let pa_0 = frame_0.phys_addr();
    let frame_1 = new_frame();

    use kerrno::KError;
    let object = AnonPrivateObject::new_root();
    let prepared = object
        .prepare_first_touch_page(0x1000)
        .expect("prepare first-touch page");
    object
        .install_page(0x1000, frame_0, PageSize::Size4K)
        .expect("install competing page");

    let err = match prepared.commit(frame_1, PageSize::Size4K) {
        Ok(_) => panic!("commit must reject a slot that became populated"),
        Err(err) => err,
    };
    assert_eq!(err, KError::AlreadyExists);
    assert_eq!(
        object
            .page_at(0x1000)
            .expect("original page must remain installed")
            .phys_addr(),
        pa_0
    );
}

#[def_test]
fn anon_private_replace_if_same_rejects_changed_slot() {
    let frame_0 = new_frame();
    let frame_1 = new_frame();
    let pa_1 = frame_1.phys_addr();
    let frame_2 = new_frame();

    let object = AnonPrivateObject::new_root();
    let expected = object
        .install_page(0x1000, frame_0, PageSize::Size4K)
        .expect("install expected page");
    object
        .replace_page(0x1000, frame_1, PageSize::Size4K)
        .expect("replace competing page");

    let result =
        object.replace_page_if_same_after(0x1000, &expected, frame_2, PageSize::Size4K, || {
            Ok::<_, ()>(())
        });
    assert!(matches!(result, Err(AnonPrivatePageCommitError::Changed)));
    assert_eq!(
        object
            .page_at(0x1000)
            .expect("competing page must remain installed")
            .phys_addr(),
        pa_1
    );
}

#[def_test]
fn anon_private_detach_rolls_back_when_not_finalized() {
    let frame_0 = new_frame();
    let pa_0 = frame_0.phys_addr();

    let object = AnonPrivateObject::new_root();
    object
        .install_page(0x1000, frame_0, PageSize::Size4K)
        .expect("install private page");

    let detached = object.detach_range(0x1000, 0x1000);
    assert!(object.page_at(0x1000).is_none());
    drop(detached);

    let page = object
        .page_at(0x1000)
        .expect("detached page must roll back");
    assert_eq!(page.phys_addr(), pa_0);
}

#[def_test]
fn anon_private_fork_prepare_rolls_back_without_commit() {
    let frame_0 = new_frame();

    let parent = AnonPrivateObject::new_root();
    let child = parent.fork_child();
    let handle = parent
        .install_page(0x1000, frame_0, PageSize::Size4K)
        .expect("install private page");

    {
        let prepared = parent
            .prepare_fork_child_pages(0x1000, 0x1000, &child)
            .expect("prepare fork pages");
        assert_eq!(prepared.pages().len(), 1);
        assert!(!handle.is_exclusive());
    }

    assert!(handle.is_exclusive());
    assert!(child.page_at(0x1000).is_none());
}

#[def_test]
fn snapshot_and_mapping_outlive_detached_object_slot() {
    let object = AnonPrivateObject::new_root();
    let page = object
        .install_page(0, new_frame(), PageSize::Size4K)
        .unwrap();
    let mapping = page.try_clone_frame().unwrap();
    let meta = kalloc::global_allocator()
        .frame_meta(mapping.phys_addr().as_usize() / khal::mem::PAGE_SIZE_4K)
        .unwrap();
    let released = object
        .detach_range(0, khal::mem::PAGE_SIZE_4K)
        .finalize_release();
    drop(released);
    drop(object);
    assert!(!page.is_exclusive());
    assert_eq!(
        meta.state(),
        kalloc::FrameState::Referenced { references: 2 }
    );
    drop(page);
    assert_eq!(
        meta.state(),
        kalloc::FrameState::Referenced { references: 1 }
    );
    drop(mapping);
}

#[def_test]
fn dropping_parent_restores_child_cow_exclusivity() {
    let parent = AnonPrivateObject::new_root();
    let page = parent
        .install_page(0, new_frame(), PageSize::Size4K)
        .unwrap();
    let child = parent.fork_child();
    parent
        .prepare_fork_child_pages(0, khal::mem::PAGE_SIZE_4K, &child)
        .unwrap()
        .commit()
        .unwrap();
    assert!(!page.is_exclusive());
    drop(parent);
    assert!(child.page_at(0).unwrap().is_exclusive());
    assert!(page.is_exclusive());
}
