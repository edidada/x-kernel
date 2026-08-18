// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut names = Vec::new();
    let _ = fs9p::fuzz::parse_dir_entries(data, &mut names);
    let _ = fs9p::fuzz::parse_dir_entries_l(data);
});
