// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network namespace ownership on [`Process`].
//!
//! A process stores only its namespace *identifier*; the namespace objects
//! (devices, routes, listen table, sockets) live in `knet`, which resolves the
//! identifier through the [`NetNsResolver`] interface provided here. Holding
//! the id instead of a shared handle keeps the shared type in the `knetns`
//! leaf crate, so neither crate depends on the other.
//!
//! The identifier is read through a plain atomic load: the resolver runs in
//! every execution context (softirq, timer, task), so the read path must
//! neither sleep nor acquire a lock that asserts on interrupt context.

use knetns::{INIT_NET_ID, NetNsId, NetNsResolver};

use crate::thread::AsThread;

/// Returns the calling context's network namespace.
///
/// Contexts without a user process (boot, softirq, kernel threads) report the
/// initial namespace, so kernel-internal networking never lands in a
/// user-created namespace.
pub fn current_net_ns() -> NetNsId {
    try_current_net_ns().unwrap_or(INIT_NET_ID)
}

/// Returns the calling context's network namespace when it runs as a user
/// thread.
pub fn try_current_net_ns() -> Option<NetNsId> {
    ktask::current()
        .try_as_thread()
        .map(|thread| thread.process().net_ns())
}

#[kiface::provide]
impl NetNsResolver {
    fn current() -> NetNsId {
        current_net_ns()
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, assert_ne, def_test};

    use super::*;
    use crate::Process;

    #[def_test]
    fn fresh_processes_start_in_the_initial_namespace() {
        let process = Process::new_init(1);
        assert_eq!(process.net_ns(), INIT_NET_ID);
    }

    #[def_test]
    fn set_net_ns_replaces_the_namespace_of_the_whole_process() {
        let process = Process::new_init(2);
        let fresh = NetNsId::from_u64(7).unwrap();
        process.set_net_ns(fresh);
        assert_eq!(process.net_ns(), fresh);
        assert_ne!(process.net_ns(), INIT_NET_ID);
    }
}
