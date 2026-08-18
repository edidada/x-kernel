// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(path) = core::str::from_utf8(data) else {
        return;
    };
    let _ = fs9p::fuzz::path_parts(path);
    let _ = fs9p::fuzz::split_parent_name(path);
});
