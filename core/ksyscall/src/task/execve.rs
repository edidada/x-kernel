// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Program execution syscalls.
//!
//! This module implements program execution operations including:
//! - Execute a new program (execve, execveat, execveeat, etc.)
//! - Program loading and initialization
//! - Argument and environment passing

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::ffi::{c_char, c_int};

use kaddr_layout::USER_HEAP_BASE;
use kerrno::{KError, KResult, LinuxError};
use kexec::{ExecRequest, ExecSource, load_user_app_request};
use khal::uspace::UserContext;
use ktask::current;
use kuaccess::vm_load_string;
use kvfs::{Filename, LookupFlags, LookupIntent};
use osvm::load_vec_until_null;

pub fn sys_execve(
    uctx: &mut UserContext,
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> KResult<isize> {
    let path = vm_load_string(path)?;
    let (args, envs) = load_strings(argv, envp)?;
    debug!("sys_execve <= path: {path:?}, args: {args:?}, envs: {envs:?}");

    let cred = kprocess::current_cred();
    run_exec(uctx, ExecRequest::from_path(path, args, envs, cred))
}

/// `execveat(2)`: execute a program selected by `dirfd`/`pathname`.
///
/// Two forms are supported, matching what runc's memfd self-reexec
/// (glibc `fexecve`) needs:
/// - `AT_EMPTY_PATH` with an empty pathname: exec the file already open on
///   `dirfd` (anonymous memfd / tmpfs file). The display path is reported as
///   `/proc/self/fd/<dirfd>`.
/// - a non-empty pathname: resolve relative to `dirfd` (`AT_FDCWD` = caller
///   cwd) and exec, following symlinks like `execve`.
///
/// `AT_SYMLINK_NOFOLLOW` is accepted but currently treated as follow (Linux
/// returns `ELOOP` for a symlink target under it; X-Kernel does not yet
/// distinguish) — documented as a follow-up.
pub fn sys_execveat(
    uctx: &mut UserContext,
    dirfd: c_int,
    pathname: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
    flags: u32,
) -> KResult<isize> {
    use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD};

    let (args, envs) = load_strings(argv, envp)?;
    let cred = kprocess::current_cred();

    // Linux requires a non-NULL pathname; `AT_EMPTY_PATH` selects the file
    // already open on `dirfd` only when the pathname is the empty string.
    if pathname.is_null() {
        return Err(KError::from(LinuxError::EFAULT));
    }
    let path = vm_load_string(pathname)?;
    let empty_path = path.is_empty();

    let request = if flags & AT_EMPTY_PATH != 0 && empty_path {
        let file = kprocess::current_resources().get_file(dirfd)?;
        let display = alloc::format!("/proc/self/fd/{dirfd}");
        debug!("sys_execveat <= fd {dirfd} (AT_EMPTY_PATH), args: {args:?}");
        ExecRequest::from_resolved_with_display(file.path().clone(), display, args, envs, cred)
    } else {
        debug!("sys_execveat <= dirfd {dirfd}, path: {path:?}, args: {args:?}");
        let location = {
            let fs_context = kprocess::current_fs_context();
            let fs = fs_context.lock();
            let cred_for_lookup = cred.clone();
            if path.as_str().chars().next().is_some_and(|c| c == '/') || dirfd == AT_FDCWD {
                Filename::new(path.as_str()).lookup_at(
                    fs.root(),
                    fs.pwd(),
                    LookupIntent::Exec,
                    LookupFlags::follow(),
                    &cred_for_lookup,
                )?
            } else {
                let dir_file = kprocess::current_resources().get_file(dirfd)?;
                if !dir_file.is_dir() {
                    return Err(KError::NotADirectory);
                }
                let snapshot = fs.clone_with_pwd(dir_file.path().clone())?;
                Filename::new(path.as_str()).lookup_at(
                    snapshot.root(),
                    snapshot.pwd(),
                    LookupIntent::Exec,
                    LookupFlags::follow(),
                    &cred_for_lookup,
                )?
            }
        };
        ExecRequest::from_resolved_with_display(location, path, args, envs, cred)
    };

    run_exec(uctx, request)
}

/// Loads `argv`/`envp` (NUL-terminated pointer arrays) into owned strings.
fn load_strings(
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> KResult<(Vec<String>, Vec<String>)> {
    let load = |ptr: *const *const c_char| -> KResult<Vec<String>> {
        if ptr.is_null() {
            return Ok(Vec::new());
        }
        load_vec_until_null(ptr)?
            .into_iter()
            .map(vm_load_string)
            .collect::<Result<Vec<_>, _>>()
    };
    Ok((load(argv)?, load(envp)?))
}

/// Resolves and loads `request`, then swaps the caller's address space and
/// register context for the new program. Shared by `execve`/`execveat`.
fn run_exec(uctx: &mut UserContext, request: ExecRequest) -> KResult<isize> {
    let curr = current();
    let thread = kprocess::current_user_thread();
    let process = thread.process().clone();

    if kprocess::scheduler::process_task_count(process.as_ref()) > 1 {
        // TODO: dispatch_irq multi-thread case
        error!("exec: multi-thread not supported");
        return Err(KError::WouldBlock);
    }

    // The display path drives process metadata (argv[0] rewrite target,
    // /proc/self/exe). For `AT_EMPTY_PATH` exec the caller already supplied
    // `/proc/self/fd/<n>`; for a pathname exec it is the supplied path.
    let args = request.args().to_vec();
    let display_path = match request.source() {
        ExecSource::Path(path) => path.clone(),
        ExecSource::Resolved {
            display_path: Some(path),
            ..
        } => path.clone(),
        ExecSource::Resolved { location, .. } => location.absolute_path()?.to_string(),
    };
    let entry_name = display_path
        .rsplit('/')
        .find(|component| !component.is_empty())
        .unwrap_or(&display_path)
        .to_string();

    let aspace_ref = process.address_space()?;
    let mut aspace = aspace_ref.lock();
    let (entry_point, user_stack_base) = load_user_app_request(&mut aspace, request)?;
    drop(aspace);

    curr.set_name(entry_name.as_str());

    let exec_update =
        kprocess::ProcessExecUpdate::new(display_path.clone(), Arc::new(args), USER_HEAP_BASE);

    #[cfg(feature = "tee")]
    let exec_update = {
        #[cfg(feature = "tee_ta_sign")]
        let ta_head_bytes = tee_task_iface::tasign::get_ta_head_cached(display_path.as_str())
            .unwrap_or_default()
            .unwrap_or_default();
        #[cfg(not(feature = "tee_ta_sign"))]
        let ta_head_bytes =
            tee_task_iface::ta_ctx::read_ta_head_if_applicable(display_path.as_str())
                .unwrap_or_default()
                .unwrap_or_default();
        exec_update.with_ta_head_bytes(ta_head_bytes)
    };

    process.apply_exec_update(exec_update)?;
    let mut exec_cred = thread.prepare_creds();
    exec_cred.apply_exec();
    thread.commit_creds(exec_cred);
    thread.reset_after_exec();

    // execve replaces the entire address space.  The old mappings are
    // now destroyed (including any System-V shared memory attachments).
    // Clear the stale ShmManager entries so the process can re-attach
    // the same segments later.
    posix_ipc::SHM_MANAGER.lock().clear_proc_shm(process.pid());

    // execve replaces the whole user register context.  Do not inherit any
    // general-purpose register of the old program: the x86_64 `execve`
    // argument `envp` sits in `rdx`, which static glibc entry reads as the
    // `rtld_fini` callback and later calls during exit handling, crashing on
    // the stale address.  Re-establish only the fields required by the target
    // architecture's ELF entry ABI.
    uctx.reset_for_exec();
    uctx.set_ip(entry_point.as_usize());
    uctx.set_sp(user_stack_base.as_usize());
    uctx.set_tls(0);
    Ok(0)
}
