// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

fn range(start: u64, end: u64) -> RecordLockRange {
    RecordLockRange::new(start, end).unwrap()
}
fn locks() -> LockRanges {
    LockRanges::default()
}

#[def_test]
fn record_ranges_split_convert_and_merge() {
    let mut held = locks();
    held.replace(1, range(0, 99), Some(RecordLockKind::Read), 7);
    held.replace(1, range(30, 59), Some(RecordLockKind::Write), 7);
    assert_eq!(held.0.len(), 3);
    let conflict = held
        .conflict(2, range(0, 99), RecordLockKind::Read)
        .unwrap();
    assert_eq!(conflict.range, range(30, 59));
    assert_eq!(conflict.pid, 7);
    assert!(
        held.conflict(1, range(0, 99), RecordLockKind::Write)
            .is_none()
    );
    held.replace(1, range(30, 59), Some(RecordLockKind::Read), 7);
    assert_eq!(held.0.len(), 1);
    assert_eq!(held.0[0].conflict.range, range(0, 99));
    held.replace(1, range(20, 79), None, 7);
    assert_eq!(held.0.len(), 2);
    assert_eq!(held.0[0].conflict.range, range(0, 19));
    assert_eq!(held.0[1].conflict.range, range(80, 99));
}

#[def_test]
fn record_ranges_share_readers_and_keep_adjacent_owners_separate() {
    let mut held = locks();
    held.replace(1, range(0, 9), Some(RecordLockKind::Read), 1);
    held.replace(2, range(0, 9), Some(RecordLockKind::Read), 2);
    assert!(
        held.conflict(3, range(0, 9), RecordLockKind::Read)
            .is_none()
    );
    assert_eq!(
        held.blockers(3, range(0, 9), RecordLockKind::Write).len(),
        2
    );
    assert!(
        held.conflict(3, range(10, 19), RecordLockKind::Write)
            .is_none()
    );
    held.replace(3, range(10, 19), Some(RecordLockKind::Read), 3);
    assert_eq!(held.0.len(), 3);
    held.replace(2, range(0, 9), None, 2);
    assert_eq!(held.0.len(), 2);
}

#[def_test]
fn record_ranges_keep_eof_locked_after_split() {
    let mut held = locks();
    held.replace(1, range(0, i64::MAX as u64), Some(RecordLockKind::Write), 1);
    held.replace(1, range(0, 9), None, 1);
    assert_eq!(held.0[0].conflict.range, range(10, i64::MAX as u64));
    held.replace(1, range(0, 9), Some(RecordLockKind::Write), 1);
    assert_eq!(held.0.len(), 1);
    assert_eq!(held.0[0].conflict.range, range(0, i64::MAX as u64));
    held.replace(1, range(0, i64::MAX as u64), None, 1);
    assert!(held.0.is_empty());
}

fn test_inode() -> Arc<VfsInode> {
    VfsInode::new_dir_with_defaults(
        crate::NodeFlags::empty(),
        crate::VfsInodeInit::new(
            1,
            0,
            crate::Umode::new(crate::NodeType::Directory, crate::NodePermission::default()),
        ),
    )
}

#[def_test(serial)]
fn record_owner_close_and_wait_cancellation_release_state() {
    let inode = test_inode();
    let first = RecordLockOwner::default();
    let second = RecordLockOwner::default();
    let full = range(0, i64::MAX as u64);
    assert!(
        first
            .set(&inode, full, Some(RecordLockKind::Write), 101, false)
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        second.set(&inode, full, Some(RecordLockKind::Read), 102, false),
        Err(KError::WouldBlock)
    ));
    let wait = second
        .set(&inode, full, Some(RecordLockKind::Read), 102, true)
        .unwrap()
        .unwrap();
    assert_eq!(REGISTRY.lock().waits[&second.0].len(), 1);
    first.release_file(&inode);
    assert!(wait.changed.has_changed_since(wait.generation));
    drop(wait);
    assert!(!REGISTRY.lock().waits.contains_key(&second.0));
    assert!(
        second
            .conflict(&inode, full, RecordLockKind::Write)
            .is_none()
    );
    assert!(!REGISTRY.lock().files.contains_key(&file_id(&inode)));
    assert!(
        second
            .set(&inode, full, Some(RecordLockKind::Write), 102, false)
            .unwrap()
            .is_none()
    );
    drop(second);
    assert!(
        first
            .conflict(&inode, full, RecordLockKind::Write)
            .is_none()
    );
}

#[def_test]
fn record_deadlock_graph_uses_live_blockers() {
    let inode = test_inode();
    let mut registry = Registry::new();
    for (id, owner) in [(11, 1), (22, 2), (33, 3)] {
        let mut ranges = locks();
        ranges.replace(
            owner,
            range(0, 9),
            Some(RecordLockKind::Write),
            owner as i32,
        );
        registry.files.insert(
            id,
            FileLocks {
                _inode: inode.clone(),
                ranges,
                changed: Arc::new(PollEvent::new()),
                waiter_count: 1,
            },
        );
    }
    registry.waits.entry(1).or_default().insert(
        100,
        PendingLock {
            file: 22,
            range: range(0, 9),
            kind: RecordLockKind::Write,
        },
    );
    registry.waits.entry(2).or_default().insert(
        101,
        PendingLock {
            file: 33,
            range: range(0, 9),
            kind: RecordLockKind::Write,
        },
    );
    assert!(registry.has_cycle(3, BTreeSet::from([1])));
    assert!(!registry.has_cycle(4, BTreeSet::from([1])));
    registry.files.get_mut(&22).unwrap().ranges.0.clear();
    assert!(!registry.has_cycle(3, BTreeSet::from([1])));
}
