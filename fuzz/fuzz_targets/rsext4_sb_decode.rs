// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rsext4::endian::DiskFormat;
use rsext4::layout::{Ext4Features, Ext4Layout};
use rsext4::superblock::Ext4Superblock;

fuzz_target!(|data: &[u8]| {
    if data.len() < Ext4Superblock::SUPERBLOCK_SIZE {
        return;
    }
    let sb = Ext4Superblock::from_disk_bytes(&data[..Ext4Superblock::SUPERBLOCK_SIZE]);
    let _ = (
        sb.is_valid(),
        sb.block_size(),
        sb.blocks_count(),
        sb.free_blocks_count(),
        sb.reserved_blocks_count(),
        sb.block_groups_count(),
        sb.blocks_per_group(),
        sb.inodes_per_group(),
        sb.inode_size(),
        sb.descs_per_block(),
        sb.get_desc_size(),
        sb.inode_table_blocks(),
        sb.has_extents(),
        sb.has_flex_bg(),
        sb.has_journal(),
        sb.unsupported_incompat_features(),
        sb.unsupported_ro_compat_features(),
        sb.has_feature_compat(Ext4Superblock::EXT4_FEATURE_COMPAT_HAS_JOURNAL),
        sb.has_feature_incompat(Ext4Superblock::EXT4_FEATURE_INCOMPAT_EXTENTS),
        sb.has_feature_incompat(Ext4Superblock::EXT4_FEATURE_INCOMPAT_64BIT),
        sb.has_feature_ro_compat(0),
    );
    let _ = Ext4Features::from_superblock(&sb);
    if let Ok(layout) = Ext4Layout::try_from_superblock(&sb) {
        let _ = (
            layout.has_flex_bg(),
            layout.flex_bg_size(),
            layout.flex_group_of(0),
        );
    }
});
