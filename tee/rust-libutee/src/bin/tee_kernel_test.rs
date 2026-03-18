// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use rust_utee::syscalls::syscall_table::_utee_test;

fn main() {
    let res = unsafe { _utee_test() };
    assert_eq!(res, 0);
}
