// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process capability and control syscalls.
//!
//! This module implements process control and capability operations including:
//! - Process capabilities (capget, capset, etc.)
//! - Process resource limits (prlimit, etc.)
//! - Process information queries

use core::ffi::c_char;

use kerrno::{KError, KResult};
use ktask::current;
use kthread::{current_process_state, get_process_state};
use kuaccess::vm_load_string;
use linux_raw_sys::general::{__user_cap_data_struct, __user_cap_header_struct};
use osvm::write_vm_mem;
use posix_types::UserPtr;

const CAPABILITY_VERSION_3: u32 = 0x20080522;
/// `_LINUX_CAPABILITY_U32S_3`: two 32-bit words cover the V3 set.
const CAPABILITY_U32S_V3: usize = 2;

fn validate_cap_header(
    header_ptr: UserPtr<__user_cap_header_struct>,
) -> KResult<__user_cap_header_struct> {
    let mut header = header_ptr.read_vm()?;
    if header.version != CAPABILITY_VERSION_3 {
        header.version = CAPABILITY_VERSION_3;
        header_ptr.write_vm(header)?;
        return Err(KError::InvalidInput);
    }
    let _ = get_process_state(header.pid as u32)?;
    Ok(header)
}

fn is_cap_header_self(header_pid: i32) -> bool {
    header_pid == 0 || header_pid as u32 == current_process_state().proc.pid()
}

/// Capability words that match the kernel's `euid == 0` privilege model.
///
/// There is no per-bit capability state yet. Privileged processes report a
/// full set so `capget` agrees with `is_privileged()`; everyone else reports
/// empty sets so userspace cannot skip checks after seeing `u32::MAX`.
fn reported_cap_words(is_privileged: bool) -> [__user_cap_data_struct; CAPABILITY_U32S_V3] {
    let bits = if is_privileged { u32::MAX } else { 0 };
    let word = __user_cap_data_struct {
        effective: bits,
        permitted: bits,
        inheritable: bits,
    };
    [word; CAPABILITY_U32S_V3]
}

/// Return the capability sets of the process named in `header`.
///
/// Sets follow `Credentials::is_privileged` (`euid == 0` for Linux
/// capabilities). Querying another process requires the same access rule as
/// `/proc/<pid>/fd` (`can_access_process_files`). A null `data` pointer is
/// accepted so userspace can probe the supported ABI version.
pub fn sys_capget(
    header: UserPtr<__user_cap_header_struct>,
    data: UserPtr<__user_cap_data_struct>,
) -> KResult<isize> {
    let header = validate_cap_header(header)?;
    if data.is_null() {
        return Ok(0);
    }

    let target = get_process_state(header.pid as u32)?;
    // Clone so a self-query does not nest `credentials` reads, and so two
    // processes querying each other cannot ABBA-deadlock the RwLocks.
    let caller_cred = current_process_state().credentials.read().clone();
    let target_cred = target.credentials.read().clone();
    if !caller_cred.can_access_process_files(&target_cred, is_cap_header_self(header.pid)) {
        return Err(KError::OperationNotPermitted);
    }
    let words = reported_cap_words(target_cred.is_privileged());
    data.write_vm_slice(&words)?;
    Ok(0)
}

/// `capset` does not store a capability mask; privilege stays `euid == 0`.
///
/// Only the calling process may be named. The call succeeds so existing
/// drop-capabilities sequences keep working, but a later `capget` still
/// reports the `is_privileged()` approximation.
pub fn sys_capset(
    header: UserPtr<__user_cap_header_struct>,
    _data: UserPtr<__user_cap_data_struct>,
) -> KResult<isize> {
    let header = validate_cap_header(header)?;
    if !is_cap_header_self(header.pid) {
        return Err(KError::OperationNotPermitted);
    }
    Ok(0)
}

pub fn sys_get_mempolicy(
    _policy: *mut i32,
    _nodemask: *mut usize,
    _maxnode: usize,
    _addr: usize,
    _flags: usize,
) -> KResult<isize> {
    warn!("Dummy get_mempolicy called");
    Ok(0)
}

/// prctl() is called with a first argument describing what to do, and further
/// arguments with a significance depending on the first one.
/// The first argument can be:
/// - PR_SET_NAME: set the name of the calling thread, using the value pointed to by `arg2`
/// - PR_GET_NAME: get the name of the calling
/// - PR_SET_SECCOMP: not implemented; returns `EINVAL` like Linux without `CONFIG_SECCOMP`
/// - PR_MCE_KILL: set the machine check exception policy
/// - PR_SET_MM options: set various memory management options (start/end code/data/brk/stack)
pub fn sys_prctl(
    option: u32,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> KResult<isize> {
    use linux_raw_sys::prctl::*;

    debug!("sys_prctl <= option: {option}, args: {arg2}, {arg3}, {arg4}, {arg5}");

    match option {
        PR_SET_NAME => {
            let s = vm_load_string(arg2 as *const c_char)?;
            current().set_name(&s);
        }
        PR_GET_NAME => {
            let name = current().name();
            let len = name.len().min(15);
            let mut buf = [0; 16];
            buf[..len].copy_from_slice(&name.as_bytes()[..len]);
            write_vm_mem(arg2 as _, &buf)?;
        }
        PR_SET_SECCOMP => {
            // Do not report success: no filter is installed.
            return Err(KError::InvalidInput);
        }
        PR_MCE_KILL => {}
        PR_SET_VMA => {
            // Allow user space to set anonymous VMA names (e.g. Go runtime).
            // We currently do not persist VMA metadata, but returning success
            // keeps behavior compatible with Linux for this common path.
            if arg2 as u32 != PR_SET_VMA_ANON_NAME {
                return Err(KError::InvalidInput);
            }
        }
        PR_SET_MM => {
            // not implemented; but avoid annoying warnings
            return Err(KError::InvalidInput);
        }
        _ => {
            warn!("sys_prctl: unsupported option {option}");
            return Err(KError::InvalidInput);
        }
    }

    Ok(0)
}
