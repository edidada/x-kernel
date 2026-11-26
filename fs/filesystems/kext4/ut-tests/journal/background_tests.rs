// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mounted journal timer, pressure, and teardown regressions.

use alloc::{sync::Arc, vec::Vec};

use ktime_types::TimeSpan;
use kvfs::{Mount, SuperBlock, SuperBlockFlags};
use unittest::def_test;

use super::{
    JournalCredits, MountedJournal, TransactionId,
    tests::revoke_journal_test_filesystem_with_log_blocks,
};
use crate::{
    Ext4Error, Ext4SbInfo, FilesystemBlock,
    inode::allocation_test_inode,
    superblock::test_fixtures::{TEST_BLOCK_SIZE, TestDevice, allocate_checkpointed_regular_inode},
    vfs::{EXT4_SUPER_OPERATIONS, Ext4Mount, Ext4SuperOperations},
};

fn mounted_background_journal(
    interval: TimeSpan,
) -> (Arc<Mount>, Arc<MountedJournal>, Arc<TestDevice>) {
    let (mut state, device) = revoke_journal_test_filesystem_with_log_blocks(900, 37);
    let inode = allocate_checkpointed_regular_inode(&mut state);
    let journal = state.metadata_journal().unwrap();
    let sb = SuperBlock::new_with_flags_and_private(
        &crate::FILE_SYSTEM_TYPE,
        &EXT4_SUPER_OPERATIONS,
        kalloc_types::KBox::new(Ext4Mount::new(state)).unwrap(),
        SuperBlockFlags::empty(),
        TEST_BLOCK_SIZE as u64,
        u64::MAX,
        |sb| {
            let inode = Ext4SuperOperations::iget_from_private(sb, inode).unwrap();
            kvfs::Dentry::new_file_from_inode(inode, None, alloc::string::String::new())
        },
    );
    let mount = sb.private::<Ext4Mount>().unwrap();
    let guard = mount.write(&sb);
    journal.start_worker(&sb, interval).unwrap();
    drop(guard);
    (Mount::new_root(&sb), journal, device)
}

fn stage_background_allocation(state: &mut Ext4SbInfo, journal: &MountedJournal) -> TransactionId {
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();
    state
        .allocate_block(
            &allocation_test_inode(12),
            Some(FilesystemBlock::new(6)),
            &mut handle,
        )
        .unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    transaction
}

fn fill_to_background_watermark(state: &mut Ext4SbInfo, journal: &MountedJournal) -> TransactionId {
    let mut append = || {
        let tid = stage_background_allocation(state, journal);
        state.commit_metadata_transaction(tid).unwrap();
        tid
    };
    for _ in 1..6 {
        append();
    }
    append()
}

#[def_test]
fn background_age_commits_idle_transaction_after_concurrent_wakeups() {
    let (active_mount, journal, _device) = mounted_background_journal(TimeSpan::from_millis(100));
    let sb = active_mount.super_block();
    let mount = sb.private::<Ext4Mount>().unwrap();
    let mut state = mount.write(sb);
    let tid = stage_background_allocation(&mut state, &journal);
    let worker = journal.worker().unwrap();
    // A claimed occurrence waits for the mount. Ready and running occurrences
    // race with concurrent notifications, then with rearming for a young tid.
    let tasks: Vec<_> = (0..4)
        .map(|_| {
            let worker = worker.clone();
            ktask::spawn(move || {
                for _ in 0..8 {
                    worker.notify_after(TimeSpan::ZERO).unwrap();
                }
            })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.join(), 0);
    }
    assert_eq!(journal.running_transaction().unwrap(), Some(tid));
    drop(state);
    journal.transactions.wait_for_commit_or_abort(tid);
    assert!(journal.is_committed(tid));
    assert_eq!(journal.pending_checkpoint_count(), 1);
    drop(active_mount);
    assert!(journal.worker().is_none());
    assert_eq!(journal.pending_checkpoint_count(), 0);
}

#[def_test]
fn background_watermark_reclaims_before_append_blocks_and_preserves_young_tid() {
    let (active_mount, journal, _device) = mounted_background_journal(TimeSpan::from_secs(600));
    let sb = active_mount.super_block();
    let mount = sb.private::<Ext4Mount>().unwrap();
    let mut state = mount.write(sb);
    // Six five-block commits fit the 36 usable blocks. No foreground append
    // encountered JournalBusy, but only six blocks remain for the next writer.
    let last = fill_to_background_watermark(&mut state, &journal);
    assert_eq!(journal.pending_checkpoint_count(), 6);
    let young = stage_background_allocation(&mut state, &journal);
    drop(state);
    journal.transactions.wait_checkpoint(last).unwrap();
    assert_eq!(journal.running_transaction().unwrap(), Some(young));
    drop(active_mount);
    assert!(journal.is_committed(young));
    assert!(!journal.superblock().has_nonzero_log_start());
    assert!(journal.worker().is_none());
}

#[def_test]
fn background_ordered_flush_failure_aborts_and_marks_mount_readonly() {
    let (active_mount, journal, device) = mounted_background_journal(TimeSpan::from_millis(20));
    let sb = active_mount.super_block();
    let mount = sb.private::<Ext4Mount>().unwrap();
    let mut state = mount.write(sb);
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let tid = handle.id();
    handle.require_ordered_data_flush().unwrap();
    state.complete_metadata_mutation(handle, Ok(())).unwrap();
    device.fail_flush_at(device.flush_count() + 1);
    drop(state);
    journal.transactions.wait_for_commit_or_abort(tid);
    // Abort wakes journal waiters before the mount guard publishes read-only.
    drop(mount.read());
    assert!(sb.flags().contains(SuperBlockFlags::RDONLY));
    assert!(journal.is_aborted());
    assert!(!journal.is_committed(tid));
    assert!(matches!(
        journal.begin(JournalCredits::new(1)),
        Err(Ext4Error::JournalAborted)
    ));
}

#[def_test]
fn background_checkpoint_failure_retains_queue_and_recovery_evidence() {
    let (active_mount, journal, device) = mounted_background_journal(TimeSpan::from_secs(600));
    let sb = active_mount.super_block();
    let mount = sb.private::<Ext4Mount>().unwrap();
    let mut state = mount.write(sb);
    let last = fill_to_background_watermark(&mut state, &journal);
    device.fail_flush_at(device.flush_count() + 1);
    drop(state);
    assert!(matches!(
        journal.transactions.wait_checkpoint(last),
        Err(Ext4Error::JournalAborted)
    ));
    drop(mount.read());
    assert!(sb.flags().contains(SuperBlockFlags::RDONLY));
    assert!(journal.is_aborted());
    assert_eq!(journal.pending_checkpoint_count(), 6);
    assert!(journal.superblock().has_nonzero_log_start());
}
