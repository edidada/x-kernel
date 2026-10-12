// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TEE syscall authorization at the kernel boundary.

use kthread::current_process_state;
use tee_raw_sys::TEE_ERROR_ACCESS_DENIED;
use tee_task_iface::TeeTaCtx;

use crate::tee::TeeResult;

/// True when the current process image is a canonical installed TA (`/tee/ta/{uuid}.ta`).
pub fn current_process_may_invoke_tee_syscalls() -> bool {
    TeeTaCtx::is_ta(&current_process_state().exe_path().read())
}

/// Rejects TEE Internal API syscalls from non-TA processes.
pub fn require_tee_ta_caller() -> TeeResult {
    if current_process_may_invoke_tee_syscalls() {
        Ok(())
    } else {
        Err(TEE_ERROR_ACCESS_DENIED)
    }
}
