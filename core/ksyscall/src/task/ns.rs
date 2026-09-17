// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace syscalls (`unshare`, `setns`).

use kerrno::{KError, KResult};
use kns::NamespaceFlags;

/// Separates namespace state from the rest of the process (`unshare(2)`).
///
/// Supported namespace types are mount, UTS, IPC, PID-for-children and network.
/// A network namespace is owned by the process (see [`kprocess::Process::
/// net_ns`]) rather than the namespace proxy, so `NEWNET` is applied after the
/// proxy switch succeeds: a fresh, isolated namespace is created up front and
/// only committed once the rest of the unshare succeeded. `NEWUSER`,
/// `NEWCGROUP` and `NEWTIME` return `EOPNOTSUPP` (mapped from
/// `KError::Unsupported`) rather than silently sharing global state.
pub fn sys_unshare(flags: u64) -> KResult<isize> {
    let namespace_flags = NamespaceFlags::from_bits_truncate(flags);
    debug!("sys_unshare <= flags: {flags:#x} ({namespace_flags:?})");

    // Create the fresh network namespace before touching current state so an
    // unsupported request changes nothing.
    let fresh_net_ns = if namespace_flags.contains(NamespaceFlags::NEWNET) {
        Some(knet::netns::create_fresh()?)
    } else {
        None
    };
    let nsproxy_flags = namespace_flags & !NamespaceFlags::NEWNET;
    kprocess::unshare_current(nsproxy_flags)?;

    if let Some(net_ns) = fresh_net_ns {
        kprocess::current_user_process().set_net_ns(net_ns.id());
    }
    Ok(0)
}

/// Joins an existing namespace via a namespace file descriptor (`setns(2)`).
///
/// Namespace fds (`/proc/[pid]/ns/*`) are not wired up yet, so any valid
/// request fails explicitly with `EOPNOTSUPP`. Invalid `nstype` values are
/// rejected with `EINVAL` before the unsupported path is reached.
pub fn sys_setns(fd: usize, nstype: u64) -> KResult<isize> {
    // `nstype` is either 0 (derive the namespace from the fd) or exactly one
    // CLONE_NEW* bitset value with no unknown bits.
    let type_bits = NamespaceFlags::from_bits_truncate(nstype);
    if nstype != 0 && (type_bits.bits() != nstype || !is_single_bit(nstype)) {
        return Err(KError::InvalidInput);
    }
    debug!("sys_setns <= fd: {fd:#x}, nstype: {nstype:#x} — unsupported (namespace fds not wired)");
    Err(KError::Unsupported)
}

fn is_single_bit(value: u64) -> bool {
    value != 0 && value & (value - 1) == 0
}
