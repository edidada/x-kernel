// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use rust_utee::trace_print;

// #[unsafe(no_mangle)]
fn main() {
    let s = String::from("Hello, world!");
    trace_print!("{}", s);
}
