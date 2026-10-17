// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace syscalls (`unshare`, `setns`).
//!
//! `unshare(2)` runs a two-phase [`NsProxy::prepare_unshare`] /
//! `Process::apply_prepared_unshare` commit so that every Linux acl and
//! combination check happens before any field of the running process changes;
//! the descriptor-table detach then commits on the calling thread. `setns(2)`
//! resolves the fd through one of two channels: an [`NsFd`] that pins the
//! namespace directly, or a pidfd whose target process's namespace is entered
//! (Linux 5.8+ semantics). Both channels sit behind one authorization gate:
//! joining a namespace requires the privileged-credential proxy of Linux's
//! per-type `CAP_SYS_ADMIN` checks.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use kns::{NamespaceFlags, NamespaceType, NsFd, UnshareContext, UnshareError};
use kprocess::{PidFd, current_files, current_user_process};
use linux_raw_sys::general::{
    CLONE_FILES, CLONE_FS, CLONE_SIGHAND, CLONE_SYSVSEM, CLONE_THREAD, CLONE_VM,
};

/// Parse `unshare(2)` flags the way Linux's flag gate does.
///
/// Unknown bits are rejected with `EINVAL`. The thread-shared-state bits
/// `CLONE_THREAD` / `CLONE_SIGHAND` / `CLONE_VM` are legal for a
/// single-threaded caller (they have nothing to detach from) and rejected only
/// for a multithreaded one, mirroring `check_unshare_flags`; Linux's
/// `CLONE_VM => CLONE_SIGHAND => CLONE_THREAD` implication chain therefore
/// never changes the outcome, because all three bits share that same rule.
/// `CLONE_NEWNS` implies `CLONE_FS`, as in `ksys_unshare`: the mount-namespace
/// switch needs a private fs struct and the kernel adds the bit itself instead
/// of requiring callers to spell it out. `CLONE_SYSVSEM` is accepted as a
/// no-op — the kernel implements no SysV-semaphore undo lists to detach, and
/// Linux's gate admits the bit unconditionally.
///
/// `NEWNET`, `NEWUSER`, `NEWCGROUP`, and `NEWTIME` pass this gate and are
/// rejected inside [`NsProxy::prepare_unshare`] with `EOPNOTSUPP` (no
/// pseudo-isolation); Linux's `CLONE_NEWUSER => CLONE_THREAD | CLONE_FS`
/// implication is moot for the same reason.
fn unshare_flag_set(flags: u64, multithreaded: bool) -> KResult<(NamespaceFlags, bool, bool)> {
    let namespace_bits = NamespaceFlags::all().bits();
    let allowed = namespace_bits
        | CLONE_FS as u64
        | CLONE_FILES as u64
        | CLONE_VM as u64
        | CLONE_SIGHAND as u64
        | CLONE_THREAD as u64
        | CLONE_SYSVSEM as u64;
    if flags & !allowed != 0 {
        return Err(KError::InvalidInput);
    }
    let shares_thread_state =
        flags & (CLONE_VM as u64 | CLONE_SIGHAND as u64 | CLONE_THREAD as u64) != 0;
    if shares_thread_state && multithreaded {
        return Err(KError::InvalidInput);
    }
    let namespaces = NamespaceFlags::from_bits_truncate(flags);
    let detach_fs = flags & CLONE_FS as u64 != 0 || namespaces.contains(NamespaceFlags::NEWNS);
    let detach_files = flags & CLONE_FILES as u64 != 0;
    Ok((namespaces, detach_fs, detach_files))
}

fn map_unshare_error(err: UnshareError) -> KError {
    match err {
        UnshareError::NotPermitted => KError::OperationNotPermitted,
        UnshareError::InvalidArgument => KError::InvalidInput,
        UnshareError::Unsupported => KError::OperationNotSupported,
        UnshareError::Mount(err) => err,
    }
}

/// Disconnect the calling process from parts of its namespace/share state
/// (`unshare(2)`).
///
/// All validation happens in the prepare phase (privilege, `NEWNS` with a
/// shared `fs_struct`, unsupported namespace kinds). The commit phase installs
/// the prepared bundle on the process, then unshares the calling thread's
/// descriptor table when `CLONE_FILES` was requested — the fd table is
/// thread-owned, so the detach must run on this thread's own binding.
pub(crate) fn sys_unshare(flags: usize) -> KResult<isize> {
    let process = current_user_process();
    let multithreaded = process.threads().len() > 1;
    let (namespace_flags, detach_fs, detach_files) = unshare_flag_set(flags as u64, multithreaded)?;

    let nsproxy = process.nsproxy()?;
    let fs_context = process.fs_context()?;
    let prepared = nsproxy
        .prepare_unshare(
            namespace_flags,
            UnshareContext {
                privileged: kprocess::current_cred().has_sys_admin(),
                fs_shared_with_others: process.fs_is_shared(),
                unshare_fs: detach_fs,
                unshare_files: detach_files,
            },
            &fs_context.lock(),
        )
        .map_err(map_unshare_error)?;
    let unshare_files = prepared.unshare_files();
    process.apply_prepared_unshare(prepared)?;
    if unshare_files {
        current_files().unshare_fd_table()?;
    }
    Ok(0)
}

/// Join a namespace referenced by a namespace fd or a pidfd (`setns(2)`).
///
/// The caller must hold `CAP_SYS_ADMIN` authority (the euid-0 proxy standing
/// in for Linux's per-type install checks); joining any namespace without
/// it is `EPERM`. The bad-fd error keeps priority by resolving the fd first.
///
/// The `NsFd` channel requires `nstype` to be zero or to match the fd's
/// namespace type (`EINVAL` otherwise). The `PidFd` channel implements the
/// Linux 5.8 semantics: `nstype` is an OR-combination mask, every requested
/// slot is validated against the pidfd's target process, and the whole set is
/// committed before the caller observes any change.
pub(crate) fn sys_setns(fd: usize, nstype: usize) -> KResult<isize> {
    let process = current_user_process();
    let file = current_files().get_file(fd as i32)?;
    if !kprocess::current_cred().has_sys_admin() {
        return Err(KError::OperationNotPermitted);
    }

    if let Ok(nsfd) = NsFd::from_file(&file) {
        return sys_setns_nsfd(&process, nsfd, nstype as u64);
    }
    if let Ok(pidfd) = PidFd::from_file(&file) {
        return sys_setns_pidfd(&process, pidfd, nstype as u64);
    }
    Err(KError::InvalidInput)
}

/// `setns` through the namespace-fd channel.
fn sys_setns_nsfd(process: &kprocess::Process, nsfd: Arc<NsFd>, nstype: u64) -> KResult<isize> {
    let fd_type = nsfd.ns_type();
    if nstype != 0 && nstype != nsfd.namespace_flag().bits() {
        return Err(KError::InvalidInput);
    }
    // Linux rejects setns(CLONE_NEWPID) from a multithreaded process (EINVAL).
    if fd_type == NamespaceType::Pid && process.threads().len() > 1 {
        return Err(KError::InvalidInput);
    }

    let target_mnt = match &*nsfd {
        NsFd::Mnt(ns) => Some(ns.clone()),
        _ => None,
    };
    let new_proxy = process
        .nsproxy()?
        .with_namespace(&nsfd)
        .map_err(map_unshare_error)?;
    process.apply_setns(new_proxy, target_mnt)?;
    Ok(0)
}

/// `setns` through the pidfd channel (Linux 5.8+).
///
/// `nstype` is a mask: each `CLONE_NEW*` bit selects one namespace of the
/// target process to enter. Namespace kinds without isolated backing tables
/// answer `EOPNOTSUPP` instead of silently half-switching.
fn sys_setns_pidfd(process: &kprocess::Process, pidfd: Arc<PidFd>, nstype: u64) -> KResult<isize> {
    let requested = NamespaceFlags::from_bits(nstype).ok_or(KError::InvalidInput)?;
    if requested.is_empty() {
        return Err(KError::InvalidInput);
    }
    let supported = NamespaceFlags::NEWNS
        | NamespaceFlags::NEWUTS
        | NamespaceFlags::NEWIPC
        | NamespaceFlags::NEWPID;
    if (requested & supported) != requested {
        return Err(KError::OperationNotSupported);
    }
    if requested.contains(NamespaceFlags::NEWPID) && process.threads().len() > 1 {
        return Err(KError::InvalidInput);
    }

    // Validate-and-commit per slot: every requested namespace is resolved
    // against the target process and swapped into a successively derived
    // proxy, so a rejected kind fails the whole call before any commit.
    let target = pidfd.live_process()?;
    let mut proxy = process.nsproxy()?;
    for flag in [
        NamespaceFlags::NEWNS,
        NamespaceFlags::NEWUTS,
        NamespaceFlags::NEWIPC,
        NamespaceFlags::NEWPID,
    ] {
        if !requested.contains(flag) {
            continue;
        }
        let resolved = if flag == NamespaceFlags::NEWNS {
            target.mnt_ns().map(NsFd::Mnt)
        } else if flag == NamespaceFlags::NEWUTS {
            target.uts_ns().map(NsFd::Uts)
        } else if flag == NamespaceFlags::NEWIPC {
            target.ipc_ns().map(NsFd::Ipc)
        } else {
            target.pid_ns_for_children().map(NsFd::Pid)
        }?;
        proxy = proxy.with_namespace(&resolved).map_err(map_unshare_error)?;
    }

    let target_mnt = if requested.contains(NamespaceFlags::NEWNS) {
        Some(target.mnt_ns()?)
    } else {
        None
    };
    process.apply_setns(proxy, target_mnt)?;
    Ok(0)
}

#[cfg(unittest)]
mod tests_namespace {
    use linux_raw_sys::general::{
        CLONE_NEWCGROUP, CLONE_NEWIPC, CLONE_NEWNET, CLONE_NEWNS, CLONE_NEWPID, CLONE_NEWTIME,
        CLONE_NEWUSER, CLONE_NEWUTS,
    };
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn unshare_flag_set_parses_supported_namespace_and_detach_flags() {
        let raw = (CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWPID) as u64
            | CLONE_FS as u64
            | CLONE_FILES as u64;
        let (namespaces, detach_fs, detach_files) = unshare_flag_set(raw, false).unwrap();
        assert!(namespaces.contains(NamespaceFlags::NEWNS));
        assert!(namespaces.contains(NamespaceFlags::NEWUTS));
        assert!(namespaces.contains(NamespaceFlags::NEWIPC));
        assert!(namespaces.contains(NamespaceFlags::NEWPID));
        assert!(detach_fs && detach_files);
    }

    #[def_test]
    fn unshare_flag_set_rejects_unknown_bits() {
        assert!(unshare_flag_set(1 << 0, false).is_err());
        assert!(unshare_flag_set(u64::MAX, false).is_err());
    }

    #[def_test]
    fn unshare_flag_set_keeps_explicitly_unsupported_namespace_flags() {
        // NEWNET/NEWUSER/NEWCGROUP/NEWTIME pass the flag-set gate and are
        // rejected with EOPNOTSUPP in the prepare phase instead.
        for raw in [
            CLONE_NEWNET as u64,
            CLONE_NEWUSER as u64,
            CLONE_NEWCGROUP as u64,
            CLONE_NEWTIME as u64,
        ] {
            let (namespaces, ..) = unshare_flag_set(raw, false).unwrap();
            assert_eq!(namespaces.bits(), raw);
        }
    }

    #[def_test]
    fn unshare_flag_set_newns_implies_clone_fs() {
        // Linux's ksys_unshare adds CLONE_FS itself when CLONE_NEWNS is given;
        // callers never need to spell it out.
        let (namespaces, detach_fs, detach_files) =
            unshare_flag_set(CLONE_NEWNS as u64, true).unwrap();
        assert!(namespaces.contains(NamespaceFlags::NEWNS));
        assert!(detach_fs);
        assert!(!detach_files);
    }

    #[def_test]
    fn unshare_flag_set_accepts_thread_state_bits_only_for_single_thread() {
        let thread_bits = (CLONE_VM | CLONE_SIGHAND | CLONE_THREAD) as u64;
        // A single-threaded caller has nothing to detach: the bits are no-ops.
        let (namespaces, detach_fs, detach_files) =
            unshare_flag_set(thread_bits | CLONE_NEWNS as u64, false).unwrap();
        assert!(namespaces.contains(NamespaceFlags::NEWNS));
        assert!(detach_fs && !detach_files);
        // A multithreaded caller is rejected with EINVAL, as in Linux.
        assert!(unshare_flag_set(thread_bits, true).is_err());
    }
}
