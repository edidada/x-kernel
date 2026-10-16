// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace syscalls (`unshare`, `setns`).
//!
//! `unshare(2)` runs a two-phase [`NsProxy::prepare_unshare`] /
//! `Process::apply_prepared_unshare` commit so that every Linux acl and
//! combination check happens before any field of the running process changes.
//! `setns(2)` resolves the fd through one of two channels: an [`NsFd`] that
//! pins the namespace directly, or a pidfd whose target process's namespace is
//! entered (Linux 5.8+ semantics).

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use kns::{NamespaceFlags, NamespaceType, NsFd, UnshareContext, UnshareError};
use kprocess::{PidFd, current_files, current_user_process};
use linux_raw_sys::general::{CLONE_FILES, CLONE_FS};

/// All flags `unshare(2)` accepts: namespace creation bits plus the
/// `CLONE_FS` / `CLONE_FILES` sharing-detach bits.
///
/// `NEWNET`, `NEWUSER`, `NEWCGROUP`, and `NEWTIME` are rejected inside
/// [`NsProxy::prepare_unshare`] with `EOPNOTSUPP` (no pseudo-isolation), while
/// unknown bits are rejected here with `EINVAL`.
fn unshare_flag_set(flags: u64) -> KResult<(NamespaceFlags, bool, bool)> {
    let namespace_bits = NamespaceFlags::all().bits();
    let allowed = namespace_bits | CLONE_FS as u64 | CLONE_FILES as u64;
    if flags & !allowed != 0 {
        return Err(KError::InvalidInput);
    }
    let namespaces = NamespaceFlags::from_bits_truncate(flags);
    let detach_fs = flags & CLONE_FS as u64 != 0;
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
/// shared `fs_struct`, unsupported namespace kinds); the commit phase only
/// installs the prepared bundle.
pub(crate) fn sys_unshare(flags: usize) -> KResult<isize> {
    let (namespace_flags, detach_fs, detach_files) = unshare_flag_set(flags as u64)?;

    let process = current_user_process();
    let nsproxy = process.nsproxy()?;
    let fs_context = process.fs_context()?;
    let prepared = nsproxy
        .prepare_unshare(
            namespace_flags,
            UnshareContext {
                privileged: kprocess::current_cred().is_privileged(),
                fs_shared_with_others: process.fs_is_shared(),
                unshare_fs: detach_fs,
                unshare_files: detach_files,
            },
            &fs_context.lock(),
        )
        .map_err(map_unshare_error)?;
    process.apply_prepared_unshare(prepared)?;
    Ok(0)
}

/// Join a namespace referenced by a namespace fd or a pidfd (`setns(2)`).
///
/// The `NsFd` channel requires `nstype` to be zero or to match the fd's
/// namespace type (`EINVAL` otherwise). The `PidFd` channel implements the
/// Linux 5.8 semantics: exactly one `CLONE_NEW*` bit must be given, and the
/// caller joins that namespace of the pidfd's target process.
pub(crate) fn sys_setns(fd: usize, nstype: usize) -> KResult<isize> {
    let process = current_user_process();
    let file = current_files().get_file(fd as i32)?;

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
fn sys_setns_pidfd(process: &kprocess::Process, pidfd: Arc<PidFd>, nstype: u64) -> KResult<isize> {
    let namespace = NamespaceFlags::from_bits(nstype).ok_or(KError::InvalidInput)?;
    if !namespace.bits().is_power_of_two() {
        return Err(KError::InvalidInput);
    }
    if namespace.contains(NamespaceFlags::NEWPID) && process.threads().len() > 1 {
        return Err(KError::InvalidInput);
    }

    let target = pidfd.live_process()?;
    let (replacement, target_mnt): (NsFd, Option<Arc<kns::MntNamespace>>) = match namespace {
        NamespaceFlags::NEWNS => {
            let ns = target.mnt_ns()?;
            (NsFd::Mnt(ns.clone()), Some(ns))
        }
        NamespaceFlags::NEWUTS => (NsFd::Uts(target.uts_ns()?), None),
        NamespaceFlags::NEWIPC => (NsFd::Ipc(target.ipc_ns()?), None),
        NamespaceFlags::NEWPID => (NsFd::Pid(target.pid_ns_for_children()?), None),
        _ => return Err(KError::OperationNotSupported),
    };

    let new_proxy = process
        .nsproxy()?
        .with_namespace(&replacement)
        .map_err(map_unshare_error)?;
    process.apply_setns(new_proxy, target_mnt)?;
    Ok(0)
}

#[cfg(unittest)]
mod tests_namespace {
    use linux_raw_sys::general::{
        CLONE_NEWIPC, CLONE_NEWNET, CLONE_NEWNS, CLONE_NEWPID, CLONE_NEWUTS,
    };
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn unshare_flag_set_parses_supported_namespace_and_detach_flags() {
        let raw = (CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWPID) as u64
            | CLONE_FS as u64
            | CLONE_FILES as u64;
        let (namespaces, detach_fs, detach_files) = unshare_flag_set(raw).unwrap();
        assert!(namespaces.contains(NamespaceFlags::NEWNS));
        assert!(namespaces.contains(NamespaceFlags::NEWUTS));
        assert!(namespaces.contains(NamespaceFlags::NEWIPC));
        assert!(namespaces.contains(NamespaceFlags::NEWPID));
        assert!(detach_fs && detach_files);
    }

    #[def_test]
    fn unshare_flag_set_rejects_unknown_bits() {
        assert!(unshare_flag_set(1 << 0).is_err());
        assert!(unshare_flag_set(u64::MAX).is_err());
    }

    #[def_test]
    fn unshare_flag_set_keeps_explicitly_unsupported_namespace_flags() {
        // NEWNET/NEWUSER/NEWCGROUP/NEWTIME pass the flag-set gate and are
        // rejected with EOPNOTSUPP in the prepare phase instead.
        for raw in [CLONE_NEWNET as u64, CLONE_NEWIPC as u64] {
            let (namespaces, ..) = unshare_flag_set(raw).unwrap();
            assert_eq!(namespaces.bits(), raw);
        }
    }
}
