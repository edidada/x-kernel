// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec};

use unittest::def_test;

use crate::{
    Ext4Error, Ext4SyncIntent, Ext4Timestamp, UnsupportedKind,
    disk::inode::EXT4_EXTENTS_FL,
    superblock::test_fixtures::{
        TEST_BLOCK_SIZE, TEST_FREE_BLOCKS, allocate_checkpointed_regular_inode,
        install_test_internal_journal, journal_allocator_test_filesystem,
    },
};

#[def_test]
fn reverse_migration_preserves_leading_hole_and_subsequent_write_accounting() {
    let (mut filesystem, _) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 810);
    let size = (2 * TEST_BLOCK_SIZE) as u64;
    inode.set_size(size);
    let tail = vec![b'b'; TEST_BLOCK_SIZE];
    filesystem
        .writeback_ordered_at(
            &inode,
            TEST_BLOCK_SIZE as u64,
            &tail,
            size,
            Ext4SyncIntent::DataOnly,
            || true,
        )
        .unwrap();
    let allocated = inode.blocks();
    assert_eq!(allocated, (TEST_BLOCK_SIZE / 512) as u64);
    filesystem
        .migrate_inode_mapping(
            &inode,
            inode.flags() & !EXT4_EXTENTS_FL,
            Ext4Timestamp::new(1, 0),
        )
        .unwrap();
    assert!(!inode.has_extents());
    assert_eq!(inode.blocks(), allocated);
    let mut data = vec![0xff; TEST_BLOCK_SIZE * 2];
    filesystem.read_at(&inode, 0, &mut data).unwrap();
    assert!(data[..TEST_BLOCK_SIZE].iter().all(|&byte| byte == 0));
    assert_eq!(&data[TEST_BLOCK_SIZE..], tail.as_slice());
    let head = vec![b'a'; TEST_BLOCK_SIZE];
    filesystem
        .writeback_ordered_at(&inode, 0, &head, size, Ext4SyncIntent::DataOnly, || true)
        .unwrap();
    filesystem.sync_filesystem().unwrap();
    filesystem
        .discard_regular_inode_preallocations(&inode)
        .unwrap();
    filesystem.read_at(&inode, 0, &mut data).unwrap();
    assert_eq!(&data[..TEST_BLOCK_SIZE], head.as_slice());
    assert_eq!(&data[TEST_BLOCK_SIZE..], tail.as_slice());
    assert_eq!(inode.blocks(), 2 * allocated);
    let free = filesystem.free_blocks_count();
    filesystem
        .truncate_regular_inode(&inode, 0, Ext4Timestamp::new(2, 0))
        .unwrap();
    filesystem.sync_filesystem().unwrap();
    assert_eq!(inode.blocks(), 0);
    assert_eq!(filesystem.free_blocks_count(), free + 2);
}

#[def_test]
fn direct_to_extent_migration_preserves_payload_and_format_after_reload() {
    let (mut filesystem, _) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 811);
    filesystem
        .migrate_inode_mapping(
            &inode,
            inode.flags() & !EXT4_EXTENTS_FL,
            Ext4Timestamp::new(1, 0),
        )
        .unwrap();
    inode.set_size(TEST_BLOCK_SIZE as u64);
    filesystem
        .writeback_ordered_at(
            &inode,
            0,
            b"direct",
            TEST_BLOCK_SIZE as u64,
            Ext4SyncIntent::DataOnly,
            || true,
        )
        .unwrap();
    let allocated = inode.blocks();
    filesystem
        .migrate_inode_mapping(
            &inode,
            inode.flags() | EXT4_EXTENTS_FL,
            Ext4Timestamp::new(2, 0),
        )
        .unwrap();
    filesystem.sync_filesystem().unwrap();
    let reloaded = filesystem.internal_iget(inode.number()).unwrap();
    assert!(reloaded.has_extents());
    assert_eq!(reloaded.blocks(), allocated);
    let mut data = [0; 6];
    filesystem.read_at(&reloaded, 0, &mut data).unwrap();
    assert_eq!(&data, b"direct");
}

#[def_test]
fn reverse_migration_rejects_blocks_beyond_direct_slots_without_metadata_change() {
    let (mut filesystem, _) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 812);
    let size = (17 * TEST_BLOCK_SIZE) as u64;
    inode.set_size(size);
    filesystem
        .writeback_ordered_at(
            &inode,
            (16 * TEST_BLOCK_SIZE) as u64,
            b"tail",
            size,
            Ext4SyncIntent::DataOnly,
            || true,
        )
        .unwrap();
    let flags = inode.flags();
    let root = inode.raw_i_block();
    assert_eq!(
        filesystem.migrate_inode_mapping(
            &inode,
            flags & !EXT4_EXTENTS_FL,
            Ext4Timestamp::new(1, 0)
        ),
        Err(Ext4Error::Unsupported(
            UnsupportedKind::InodeMappingConversion
        ))
    );
    assert_eq!(inode.flags(), flags);
    assert_eq!(inode.raw_i_block(), root);
    filesystem.sync_filesystem().unwrap();
}

#[def_test]
fn reverse_migration_rejects_oversized_sparse_files_even_with_empty_roots() {
    for has_head_extent in [false, true] {
        let (mut filesystem, _) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
        install_test_internal_journal(&mut filesystem, 813);
        let size = (12 * TEST_BLOCK_SIZE + 1) as u64;
        filesystem
            .truncate_regular_inode(&inode, size, Ext4Timestamp::new(1, 0))
            .unwrap();
        if has_head_extent {
            filesystem
                .writeback_ordered_at(&inode, 0, b"head", size, Ext4SyncIntent::DataOnly, || true)
                .unwrap();
        }
        let flags = inode.flags();
        let root = inode.raw_i_block();
        let blocks = inode.blocks();
        let free = filesystem.free_blocks_count();
        assert_eq!(
            filesystem.migrate_inode_mapping(
                &inode,
                flags & !EXT4_EXTENTS_FL,
                Ext4Timestamp::new(2, 0),
            ),
            Err(Ext4Error::Unsupported(
                UnsupportedKind::InodeMappingConversion
            ))
        );
        assert_eq!(inode.flags(), flags);
        assert_eq!(inode.raw_i_block(), root);
        assert_eq!(inode.size(), size);
        assert_eq!(inode.blocks(), blocks);
        assert_eq!(filesystem.free_blocks_count(), free);
        filesystem.sync_filesystem().unwrap();
        let reloaded = filesystem.internal_iget(inode.number()).unwrap();
        assert!(reloaded.has_extents());
        assert_eq!(reloaded.size(), size);
    }
}

#[def_test]
fn reverse_migration_accepts_the_exact_direct_size_limit() {
    let (mut filesystem, _) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 814);
    let size = (12 * TEST_BLOCK_SIZE) as u64;
    filesystem
        .truncate_regular_inode(&inode, size, Ext4Timestamp::new(1, 0))
        .unwrap();
    filesystem
        .writeback_ordered_at(
            &inode,
            (11 * TEST_BLOCK_SIZE) as u64,
            b"last-slot",
            size,
            Ext4SyncIntent::DataOnly,
            || true,
        )
        .unwrap();
    filesystem
        .migrate_inode_mapping(
            &inode,
            inode.flags() & !EXT4_EXTENTS_FL,
            Ext4Timestamp::new(2, 0),
        )
        .unwrap();
    filesystem.sync_filesystem().unwrap();
    let reloaded = filesystem.internal_iget(inode.number()).unwrap();
    assert!(!reloaded.has_extents());
    assert_eq!(reloaded.size(), size);
    let mut data = [0; 9];
    filesystem
        .read_at(&reloaded, (11 * TEST_BLOCK_SIZE) as u64, &mut data)
        .unwrap();
    assert_eq!(&data, b"last-slot");
}
