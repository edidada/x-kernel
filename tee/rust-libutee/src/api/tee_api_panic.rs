// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{syscalls::syscall_table::_utee_panic, tee_api_types::TEE_Result};

#[unsafe(no_mangle)]
pub extern "C" fn TEE_Panic(panic_code: TEE_Result) {
    _utee_panic(panic_code as u64);
}
