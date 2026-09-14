// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process identity syscalls.
//!
//! PID/TID values returned to userspace are rendered in the calling thread's
//! active PID namespace, so processes inside a `CLONE_NEWPID` container see
//! container-local numbers (the container init sees PID 1) while internal
//! kernel bookkeeping continues to use the root/global numbers.

use kerrno::KResult;
use kprocess::current_user_thread;

/// Returns the process ID of the current process, as visible in its active
/// PID namespace.
pub fn sys_getpid() -> KResult<isize> {
    Ok(current_user_thread().pid_in_active_ns() as _)
}

/// Returns the parent process ID of the current process, as visible in the
/// caller's active PID namespace.
///
/// A parent that is not a member of the caller's PID namespace reports 0,
/// matching Linux's behavior for a parent outside a container.
pub fn sys_getppid() -> KResult<isize> {
    Ok(current_user_thread().ppid_in_active_ns() as _)
}
