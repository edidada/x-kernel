// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rsext4::entries::Ext4DirEntryInfo::parse_from_bytes(data);

    for (entry, rec_len) in rsext4::entries::DirEntryIterator::new(data) {
        let _ = (
            entry.inode,
            entry.file_type,
            entry.name.len(),
            entry.name_str(),
            entry.is_dot(),
            entry.is_dotdot(),
            rec_len,
        );
    }

    let entries = rsext4::entries::classic_dir::list_entries(data);
    for entry in &entries {
        let _ = (entry.inode, entry.file_type, entry.name_str());
    }
    let _ = rsext4::entries::classic_dir::find_entry(data, b".");
    let _ = rsext4::entries::classic_dir::find_entry(data, b"..");
    if !data.is_empty() {
        let name_len = data[0].min(32) as usize;
        if data.len() > name_len {
            let _ = rsext4::entries::classic_dir::find_entry(data, &data[..name_len]);
        }
    }

    let seed = [0x12345678u32, 0x9abcdef0, 0x13579bdf, 0x2468ace0];
    for version in 0u8..=5 {
        let _ = rsext4::entries::htree_dir::calculate_hash(b"hello", version, &seed);
        let _ = rsext4::entries::htree_dir::calculate_hash(data, version, &seed);
    }

    for end in (12..=data.len()).step_by(4).take(64) {
        let slice = &data[..end];
        let _ = rsext4::entries::Ext4DirEntryInfo::parse_from_bytes(slice);
    }
});
