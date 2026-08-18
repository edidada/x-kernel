// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some(node) = rsext4::extents_tree::ExtentTree::parse_node(data) else {
        return;
    };
    let header = node.header();
    let _ = (
        header.eh_magic,
        header.eh_entries,
        header.eh_max,
        header.eh_depth,
        header.eh_generation,
        node.is_leaf(),
    );
});
