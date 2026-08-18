// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;
use rsext4::disknode::Ext4Inode;
use rsext4::endian::DiskFormat;

fuzz_target!(|data: &[u8]| {
    let min = Ext4Inode::GOOD_OLD_INODE_SIZE as usize;
    if data.len() < min {
        return;
    }

    for &len in &[min, Ext4Inode::LARGE_INODE_SIZE as usize] {
        if data.len() < len {
            continue;
        }
        let inode = Ext4Inode::from_disk_bytes(&data[..len]);
        let _ = (
            inode.i_mode,
            inode.i_size_lo,
            inode.i_links_count,
            inode.i_flags,
            inode.i_block,
            inode.i_uid,
            inode.i_gid,
            inode.i_atime,
            inode.i_mtime,
            inode.i_ctime,
            inode.size(),
            inode.blocks_count(),
            inode.uid(),
            inode.gid(),
            inode.file_acl(),
            inode.is_dir(),
            inode.is_file(),
            inode.is_symlink(),
            inode.have_extend_header_and_use_extend(),
        );
    }
});
