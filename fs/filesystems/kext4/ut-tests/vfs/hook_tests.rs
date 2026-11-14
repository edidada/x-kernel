// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lifecycle-hook wiring: the close, eviction and shrink-truncate entry
//! points must drop an inode's speculative prealloc spaces through the real
//! VFS dispatch, not just through the allocator-level entry points.
//!
//! The spaces are seeded directly into both index sides (as the allocator
//! unit tests do): the writeback birth that would produce them lands in a
//! later commit, so these tests isolate the hook obligations - drop on evict,
//! drop on the last writable close, clip on shrink.

use alloc::{string::String, sync::Arc};

use kvfs::{Mount, OpenFlags, Path};
use unittest::def_test;

use super::*;
use crate::{
    BlockCount, BlockGroupNumber, FilesystemBlock, LogicalBlock,
    superblock::{
        lock,
        test_fixtures::{
            TEST_BLOCK_SIZE, TEST_FREE_BLOCKS, allocate_checkpointed_regular_inode,
            insert_prealloc_space, install_test_internal_journal,
            journal_allocator_test_filesystem, prealloc_count, prealloc_twin_count,
        },
    },
};

/// Mounts `private` on a writable superblock and opens it for read-write,
/// mirroring `read_tests::cached_file`. The returned file keeps one writable
/// open, so `write_count() == 1` meets the close hook's gate.
fn writable_mount(filesystem: Ext4SbInfo, private: Ext4Inode) -> (Arc<SuperBlock>, Arc<VfsFile>) {
    let super_block = SuperBlock::new_with_flags_and_private(
        &crate::FILE_SYSTEM_TYPE,
        &EXT4_SUPER_OPERATIONS,
        kalloc_types::KBox::new(Ext4Mount::new(filesystem)).unwrap(),
        SuperBlockFlags::empty(),
        PAGE_SIZE_4K as u64,
        u64::MAX,
        |super_block| {
            let inode = Ext4SuperOperations::iget_from_private(super_block, private).unwrap();
            Dentry::new_file_from_inode(inode, None, String::new())
        },
    );
    let path = Path::new(Mount::new_root(&super_block), super_block.root_dir());
    let file =
        kvfs::dentry_open(path, OpenFlags::READ_WRITE.bits(), kcred::initial_cred()).unwrap();
    (super_block, file)
}

/// Runs `f` against the mounted ext4 state under the ext4 read lock - the
/// lock the production hooks hold while they touch the prealloc indexes.
fn with_state<R>(super_block: &SuperBlock, f: impl FnOnce(&Ext4SbInfo) -> R) -> R {
    f(&ext4_private(super_block).unwrap().read())
}

#[def_test]
fn journal_abort_marks_superblock_read_only_and_rejects_buffered_writes() {
    let (mut filesystem, storage) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let private = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 8104);
    let (super_block, file) = writable_mount(filesystem, private);
    assert!(!super_block.flags().contains(SuperBlockFlags::RDONLY));
    {
        let mut state = ext4_private(&super_block).unwrap().write(&super_block);
        let journal = state.metadata_journal().unwrap();
        let mut handle = journal.begin(crate::jbd2::JournalCredits::new(1)).unwrap();
        handle
            .consume_metadata_credit(FilesystemBlock::new(6))
            .unwrap();
        let result = handle.consume_metadata_credit(FilesystemBlock::new(7));
        assert_eq!(result, Err(crate::Ext4Error::InsufficientJournalCredits));
        assert!(handle.has_updates());
        assert_eq!(
            state.complete_metadata_mutation(handle, result),
            Err(crate::Ext4Error::InvalidJournalTransaction)
        );
    }
    assert!(super_block.flags().contains(SuperBlockFlags::RDONLY));
    let stats = with_state(&super_block, |state| state.statfs().unwrap());
    let cred = kcred::initial_cred();
    let mut context = kvfs::FsContext::new_reconfigure(
        &super_block,
        None,
        Some(b"minixdf"),
        SuperBlockFlags::empty(),
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .unwrap();
    assert_eq!(
        super_block.reconfigure(&mut context),
        Err(VfsError::from(kerrno::LinuxError::EROFS).canonicalize())
    );
    assert!(super_block.flags().contains(SuperBlockFlags::RDONLY));
    assert_eq!(
        with_state(&super_block, |state| state.statfs().unwrap()),
        stats
    );
    let inode = file.inode().downcast::<Ext4Inode>().unwrap();
    let mut count = 1;
    assert_eq!(
        check_write_limit(
            ext4_private(&super_block).unwrap(),
            &super_block,
            &inode,
            0,
            &mut count
        ),
        Err(VfsError::from(kerrno::LinuxError::EROFS).canonicalize())
    );
    assert_eq!(
        with_state(&super_block, |state| state.reserve_delalloc_range(
            &inode,
            LogicalBlock::new(0),
            1
        )),
        Err(crate::Ext4Error::JournalAborted)
    );
    let writes = storage.write_count();
    assert_eq!(
        file.write(b"must not dirty the cache"),
        Err(VfsError::from(kerrno::LinuxError::EROFS).canonicalize())
    );
    assert_eq!(storage.write_count(), writes);
}

#[def_test]
fn healthy_journal_allows_remount_read_write() {
    let (mut filesystem, _storage) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let private = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 8105);
    let (super_block, _file) = writable_mount(filesystem, private);
    super_block.mark_read_only();
    let cred = kcred::initial_cred();
    let mut context = kvfs::FsContext::new_reconfigure(
        &super_block,
        None,
        None,
        SuperBlockFlags::empty(),
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .unwrap();
    super_block.reconfigure(&mut context).unwrap();
    assert!(!super_block.flags().contains(SuperBlockFlags::RDONLY));
}

#[def_test]
fn eviction_drops_every_prealloc_space() {
    let (mut filesystem, _storage) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let private = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 8101);
    let (super_block, file) = writable_mount(filesystem, private);
    let vfs_inode = file.inode().clone();
    let inode = vfs_inode.downcast::<Ext4Inode>().unwrap();
    drop(file);

    with_state(&super_block, |state| {
        insert_prealloc_space(state, &inode, 0, 6, 4, 0);
        insert_prealloc_space(state, &inode, 8, 12, 2, 0);
    });
    assert_eq!(prealloc_count(&inode), 2);

    // Eviction drops the spaces before it even looks at the link count, so
    // the hook obligation holds for unlink and for cache eviction alike.
    EXT4_SUPER_OPERATIONS.evict_inode(&vfs_inode).unwrap();

    assert_eq!(prealloc_count(&inode), 0);
    with_state(&super_block, |state| {
        assert_eq!(prealloc_twin_count(state), 0)
    });
}

#[def_test]
fn last_writable_close_releases_prealloc_spaces() {
    let (mut filesystem, _storage) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let private = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 8102);
    let (super_block, file) = writable_mount(filesystem, private);
    let vfs_inode = file.inode().clone();
    let inode = vfs_inode.downcast::<Ext4Inode>().unwrap();

    // The close gate is `writable && write_count == 1 && no delalloc`; a
    // freshly mounted file satisfies all three.
    assert!(file.mode().contains(FMode::WRITE));
    assert_eq!(vfs_inode.write_count(), 1);
    assert!(!inode.has_delalloc_reservations());

    with_state(&super_block, |state| {
        insert_prealloc_space(state, &inode, 0, 6, 4, 0)
    });
    assert_eq!(prealloc_count(&inode), 1);

    {
        let _producer = inode.lock_writeback_submission();
        <Ext4Inode as FileOperations>::release(&inode, &vfs_inode, &file).unwrap();
        assert_eq!(
            prealloc_count(&inode),
            1,
            "busy maintenance must retain its resources"
        );
    }
    <Ext4Inode as FileOperations>::release(&inode, &vfs_inode, &file).unwrap();

    assert_eq!(prealloc_count(&inode), 0);
    with_state(&super_block, |state| {
        assert_eq!(prealloc_twin_count(state), 0)
    });
}

#[def_test]
fn shrink_truncate_clips_spaces_past_the_new_eof() {
    for (is_retry, is_grow, is_write) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
    ] {
        let (mut filesystem, _storage) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let private = allocate_checkpointed_regular_inode(&mut filesystem);
        private.set_size(3 * TEST_BLOCK_SIZE as u64);
        install_test_internal_journal(&mut filesystem, 8103);
        let (super_block, file) = writable_mount(filesystem, private);
        let vfs_inode = file.inode().clone();
        let inode = vfs_inode.downcast::<Ext4Inode>().unwrap();
        let mapping = file.mapping().clone();
        drop(file);

        // [0, 5) straddles the new one-block EOF; [6, 8) lies wholly beyond it.
        with_state(&super_block, |state| {
            insert_prealloc_space(state, &inode, 0, 6, 5, 0);
            insert_prealloc_space(state, &inode, 6, 16, 2, 0);
        });

        if is_retry {
            // Stop at the durable-orphan/publication boundary, as when revocation
            // reports an error before backing cleanup. Repeating set_len must
            // consume the orphan instead of relying on a changed size comparison.
            {
                let private = ext4_private(&super_block).unwrap();
                let mut state = private.write(&super_block);
                let time = crate::Ext4Timestamp::new(1, 0);
                state
                    .prepare_regular_inode_truncate(&inode, 3 * TEST_BLOCK_SIZE as u64, time)
                    .unwrap();
                state
                    .prepare_regular_inode_truncate(&inode, TEST_BLOCK_SIZE as u64, time)
                    .unwrap();
            }
            vfs_inode.set_size(TEST_BLOCK_SIZE as u64);
            assert_eq!(inode.size(), TEST_BLOCK_SIZE as u64);
            assert!(inode.orphan_location().is_some());
        }
        if is_grow || is_write {
            mapping
                .with_folio_or_create(2, |folio| {
                    folio.data().fill(0x7c);
                    Ok(())
                })
                .unwrap();
        }
        let target_size = if is_grow { 3 } else { 1 } * TEST_BLOCK_SIZE as u64;
        let _data = vfs_inode.lock_data();
        if is_write {
            let pos = 2 * TEST_BLOCK_SIZE as u64;
            EXT4_ADDRESS_SPACE_OPERATIONS
                .write_begin(&mapping, WriteBeginRequest::new(pos, 1))
                .unwrap();
            EXT4_ADDRESS_SPACE_OPERATIONS
                .write_end(&mapping, WriteEndRequest::new(pos, 1, 0))
                .unwrap();
        } else {
            let _invalidate = mapping.lock_invalidate_exclusive();
            <Ext4Inode as InodeOperations>::set_len(&inode, &vfs_inode, target_size).unwrap();
        }
        assert_eq!(inode.size(), target_size);
        if is_grow || is_write {
            assert!(
                mapping.with_folio(2, |folio| folio.is_none()).unwrap(),
                "new mutation must first discard the failed shrink's old cached suffix"
            );
        }
        assert!(inode.orphan_location().is_none());

        // The prefix below the new EOF stays usable; the straddler's tail and the
        // whole far space die.
        assert_eq!(prealloc_count(&inode), 1);
        let kept = inode
            .prealloc_map()
            .update(|preallocs| preallocs.get(&LogicalBlock::new(0)).copied())
            .unwrap();
        assert_eq!(kept.phys_len, BlockCount::new(1));
        assert_eq!(kept.phys_start, FilesystemBlock::new(6));
        with_state(&super_block, |state| {
            assert_eq!(prealloc_twin_count(state), 1);
            let twin_len = lock(&state.allocator)
                .preallocs_by_group
                .get(&(BlockGroupNumber::new(0), FilesystemBlock::new(6)))
                .map(|twin| twin.phys_len);
            assert_eq!(twin_len, Some(BlockCount::new(1)));
        });
    }
}

#[def_test]
fn close_retains_spaces_while_delalloc_reservations_exist() {
    let (mut filesystem, _storage) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let private = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 8104);
    let (super_block, file) = writable_mount(filesystem, private);
    let vfs_inode = file.inode().clone();
    let inode = vfs_inode.downcast::<Ext4Inode>().unwrap();

    with_state(&super_block, |state| {
        insert_prealloc_space(state, &inode, 0, 6, 4, 0);
        // Establish a delalloc reservation: the close gate must now keep the
        // space so a future allocation can consume through it.
        inode
            .reserve_delalloc_range(state, LogicalBlock::new(0), 4)
            .unwrap();
    });
    assert!(inode.has_delalloc_reservations());
    assert_eq!(prealloc_count(&inode), 1);

    // Close with an outstanding reservation: spaces survive.
    <Ext4Inode as FileOperations>::release(&inode, &vfs_inode, &file).unwrap();
    assert_eq!(prealloc_count(&inode), 1);
    with_state(&super_block, |state| {
        assert_eq!(prealloc_twin_count(state), 1)
    });

    // Eviction must still clean up whatever remains.
    EXT4_SUPER_OPERATIONS.evict_inode(&vfs_inode).unwrap();
    assert_eq!(prealloc_count(&inode), 0);
    with_state(&super_block, |state| {
        assert_eq!(prealloc_twin_count(state), 0)
    });
}
