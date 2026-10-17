// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process resource-limit syscalls.

use kerrno::{KError, KResult};
use kprocess::Pid;
use krlimit::Rlimit;
use linux_raw_sys::general::{RLIM_NLIMITS, rlimit64};
use posix_types::{UserConstPtr, UserPtr};

/// Gets and/or sets resource limits for a process.
pub(crate) fn sys_prlimit64(
    pid: Pid,
    resource: u32,
    new_limit: UserConstPtr<rlimit64>,
    old_limit: UserPtr<rlimit64>,
) -> KResult<isize> {
    // Linux copies input before PID lookup and output: aliased buffers must
    // preserve the requested update, and input EFAULT precedes ESRCH.
    let new_limit = new_limit
        .check_non_null()
        .map(|value| value.read_vm())
        .transpose()?;
    let process = kprocess::resource_limits::target_process(pid)?;
    let resources = process.resources()?;
    // Return the pair replaced by this update, even when another thread updates
    // the same resource. A rejected update must not overwrite the output buffer.
    let old = resources.update_rlimit_and_get_old(
        resource,
        new_limit.map(|value| Rlimit::new(value.rlim_cur, value.rlim_max)),
    )?;
    if let Some(old_limit) = old_limit.check_non_null() {
        old_limit.write_vm(rlimit64 {
            rlim_cur: old.current,
            rlim_max: old.max,
        })?;
    }

    if let Some(new_limit) = new_limit {
        // Linux `do_prlimit()`: raising the hard limit, or setting the soft
        // limit beyond the current hard limit, requires `CAP_SYS_RESOURCE`.
        // The credential model has no capability sets yet, so root
        // (euid 0) stands in for the capability check.
        if kprocess::current_cred().euid() != 0 {
            let (_, old_max) = resources.rlimit_values(resource)?;
            if new_limit.rlim_max > old_max || new_limit.rlim_cur > old_max {
                return Err(KError::OperationNotPermitted);
            }
        }
    }
    Ok(0)
}

/// Returns the current resource limits of the calling process.
pub(crate) fn sys_getrlimit(resource: u32, old_limit: UserPtr<rlimit64>) -> KResult<isize> {
    // Legacy getrlimit validates the resource before copying its output.
    if resource >= RLIM_NLIMITS {
        return Err(KError::InvalidInput);
    }
    if old_limit.check_non_null().is_none() {
        return Err(KError::BadAddress);
    }
    sys_prlimit64(0, resource, UserConstPtr::from(0usize), old_limit)
}

/// Updates the resource limits of the calling process.
pub(crate) fn sys_setrlimit(resource: u32, new_limit: UserConstPtr<rlimit64>) -> KResult<isize> {
    if new_limit.check_non_null().is_none() {
        return Err(KError::BadAddress);
    }
    sys_prlimit64(0, resource, new_limit, UserPtr::from(0usize))
}

unittest::path_mod_test!(tests, "../../ut-tests/task/limits.rs");
