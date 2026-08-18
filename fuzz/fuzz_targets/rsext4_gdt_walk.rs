// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fn walk_gdt(payload: &[u8], desc_size: usize, group_count: u32) {
    if desc_size == 0 || group_count == 0 || payload.is_empty() {
        return;
    }

    let table =
        rsext4::blockgroup_description::BlockGroupDescTable::new(payload, desc_size, group_count);
    let _ = (table.group_count(), table.desc_size(), table.total_free_blocks());
    for idx in 0..table.group_count() {
        if let Some(desc) = table.get_desc(idx) {
            let _ = (
                desc.block_bitmap(),
                desc.inode_bitmap(),
                desc.inode_table(),
                desc.free_blocks_count(),
                desc.free_inodes_count(),
                desc.used_dirs_count(),
                desc.bg_flags,
                desc.is_uninit_bg(),
                desc.is_block_bitmap_uninit(),
                desc.is_inode_bitmap_uninit(),
                desc.is_inode_table_zeroed(),
            );
        }
    }
    for desc in table.iter() {
        let _ = (desc.block_bitmap(), desc.inode_table(), desc.free_blocks_count());
    }
}

fuzz_target!(|data: &[u8]| {
    if let Some((meta, payload)) = data.split_first() {
        let desc_size = if meta & 1 == 0 { 32usize } else { 64usize };
        let want = u32::from(meta >> 1).saturating_add(1).min(64);
        let have = (payload.len() / desc_size).min(64) as u32;
        let group_count = want.min(have).max(if have > 0 { 1 } else { 0 });
        walk_gdt(payload, desc_size, group_count);
    }

    for desc_size in [32usize, 64] {
        let group_count = (data.len() / desc_size).min(64) as u32;
        walk_gdt(data, desc_size, group_count);
    }
});
