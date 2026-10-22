// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use khal::{paging::{MappingFlags, PageSize}, trap::PageFaultFlags};
use memaddr::{VirtAddr, PAGE_SIZE_4K};
use unittest::def_test;

use super::*;

const START: usize = 0x40_0000;
const FLAGS: MappingFlags = MappingFlags::READ.union(MappingFlags::WRITE);

fn input() -> FaultInput {
    FaultInput::new(VirtAddr::from_usize(START), PageFaultFlags::WRITE)
}

fn new_space() -> Mutex<MmSpace> {
    let start = VirtAddr::from_usize(START);
    let mut space = MmSpace::new_empty_user(start, PAGE_SIZE_4K * 4).unwrap();
    space.map(
        start,
        PAGE_SIZE_4K * 2,
        FLAGS,
        false,
        VmRuntimeRef::new_anon_private(start, PageSize::Size4K),
    ).unwrap();
    Mutex::new(space)
}

fn prepare(backend: &PrivateBackend) -> KResult<PreparedPrivatePage> {
    PreparedPrivatePage::new(BackendOps::page_size(backend))
}

fn assert_unmapped(space: &Mutex<MmSpace>) {
    assert!(matches!(
        space.lock().pgtbl.query(input().address()),
        Err(PagingError::NotMapped)
    ));
}

#[def_test]
fn first_touch_allocates_and_zeros_outside_the_address_space_mutex() {
    let space = new_space();
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |backend| {
            let available = space.try_lock().expect("preparation must release the mutex");
            drop(available);
            prepare(backend)
        }),
        PageFaultOutcome::Resolved
    );
    let mut bytes = [0xff; 64];
    space.lock().read(input().address(), &mut bytes).unwrap();
    assert_eq!(bytes, [0; 64]);
}

#[def_test]
fn first_touch_unmap_during_preparation_cannot_resurrect_the_mapping() {
    let space = new_space();
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |backend| {
            space.lock().unmap(input().address(), PAGE_SIZE_4K * 2).unwrap();
            prepare(backend)
        }),
        PageFaultOutcome::Retry
    );
    assert_unmapped(&space);
    assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Unmapped);
}

#[def_test]
fn first_touch_reused_address_rejects_the_old_object() {
    let space = new_space();
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |backend| {
            {
                let mut current = space.lock();
                current.unmap(input().address(), PAGE_SIZE_4K * 2).unwrap();
                current.map(
                    input().address(),
                    PAGE_SIZE_4K * 2,
                    FLAGS,
                    false,
                    VmRuntimeRef::new_anon_private(input().address(), PageSize::Size4K),
                ).unwrap();
            }
            prepare(backend)
        }),
        PageFaultOutcome::Retry
    );
    assert_unmapped(&space);
    assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Resolved);
}

#[def_test]
fn first_touch_permission_revocation_prevents_stale_write_mapping() {
    let space = new_space();
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |backend| {
            space.lock().protect(
                input().address(), PAGE_SIZE_4K * 2, MappingFlags::READ,
            ).unwrap();
            prepare(backend)
        }),
        PageFaultOutcome::Retry
    );
    assert_unmapped(&space);
    assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::AccessDenied);
}

#[def_test]
fn first_touch_vma_split_retries_with_current_coordinates() {
    let space = new_space();
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |backend| {
            space.lock().unmap(input().address() + PAGE_SIZE_4K, PAGE_SIZE_4K).unwrap();
            prepare(backend)
        }),
        PageFaultOutcome::Retry
    );
    assert_unmapped(&space);
    assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Resolved);
}

#[def_test]
fn first_touch_duplicate_commit_preserves_the_winner_data() {
    let space = new_space();
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |backend| {
            assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Resolved);
            space.lock().write(input().address(), &[0x5a; 64]).unwrap();
            prepare(backend)
        }),
        PageFaultOutcome::Retry
    );
    assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Resolved);
    let mut bytes = [0; 64];
    space.lock().read(input().address(), &mut bytes).unwrap();
    assert_eq!(bytes, [0x5a; 64]);
}

#[def_test]
fn first_touch_allocation_failure_publishes_nothing_and_can_retry() {
    let space = new_space();
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |_| Err(KError::NoMemory)),
        PageFaultOutcome::OutOfMemory
    );
    assert_unmapped(&space);
    assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Resolved);
}

#[def_test]
fn first_touch_state_change_takes_precedence_over_speculative_oom() {
    let space = new_space();
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |_| {
            assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Resolved);
            Err(KError::NoMemory)
        }),
        PageFaultOutcome::Retry
    );
    assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Resolved);
}

#[def_test]
fn first_touch_discard_before_commit_materializes_fresh_zeroes() {
    let space = new_space();
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |backend| {
            let page = prepare(backend)?;
            assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Resolved);
            space.lock().write(input().address(), &[0x5a; 64]).unwrap();
            space.lock().madvise_dontneed(input().address(), PAGE_SIZE_4K).unwrap();
            Ok(page)
        }),
        PageFaultOutcome::Resolved
    );
    let mut bytes = [0xff; 64];
    space.lock().read(input().address(), &mut bytes).unwrap();
    assert_eq!(bytes, [0; 64]);
}

#[def_test]
fn first_touch_nonempty_object_uses_its_existing_page() {
    let space = new_space();
    assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Resolved);
    space.lock().write(input().address(), &[0x6b; 64]).unwrap();
    {
        let mut current = space.lock();
        let mut pgtbl = current.pgtbl.modify();
        pgtbl.unmap(input().address()).unwrap();
        pgtbl.finish();
    }
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |_| panic!("must use the object page")),
        PageFaultOutcome::Resolved
    );
    let mut bytes = [0; 64];
    space.lock().read(input().address(), &mut bytes).unwrap();
    assert_eq!(bytes, [0x6b; 64]);
}

#[def_test]
fn first_touch_clear_during_preparation_cannot_revive_an_exited_mm() {
    let space = new_space();
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |backend| {
            space.lock().clear();
            prepare(backend)
        }),
        PageFaultOutcome::Retry
    );
    assert_unmapped(&space);
    assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Unmapped);
}

#[def_test]
fn first_touch_preparation_does_not_publish_pages_into_a_fork_child() {
    let space = new_space();
    let mut child = None;
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |backend| {
            let page = prepare(backend)?;
            child = Some(space.lock().try_clone().unwrap());
            Ok(page)
        }),
        PageFaultOutcome::Resolved
    );
    space.lock().write(input().address(), &[0x7c; 64]).unwrap();
    let child = child.unwrap();
    assert_eq!(MmSpace::resolve_fault(&child, input()), PageFaultOutcome::Resolved);
    let mut bytes = [0xff; 64];
    child.lock().read(input().address(), &mut bytes).unwrap();
    assert_eq!(bytes, [0; 64]);
}

#[def_test]
fn first_touch_dispatch_preserves_locked_cow_for_existing_pages() {
    let space = new_space();
    assert_eq!(MmSpace::resolve_fault(&space, input()), PageFaultOutcome::Resolved);
    space.lock().write(input().address(), &[0x3d; 64]).unwrap();
    let child = space.lock().try_clone().unwrap();
    assert_eq!(
        MmSpace::resolve_fault_with(&space, input(), |_| panic!("COW stays on the locked path")),
        PageFaultOutcome::Resolved
    );
    space.lock().write(input().address(), &[0x7e; 64]).unwrap();
    let mut bytes = [0; 64];
    child.lock().read(input().address(), &mut bytes).unwrap();
    assert_eq!(bytes, [0x3d; 64]);
}
