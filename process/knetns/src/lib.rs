// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared network-namespace identity contract.
//!
//! This leaf crate carries only the two pieces that the process and network
//! subsystems must agree on without either depending on the other:
//!
//! - [`NetNsId`], the namespace identifier a process holds;
//! - [`NetNsResolver`], the interface the process subsystem provides so the
//!   network stack can look up the caller's namespace in any context.
//!
//! The namespace objects themselves (device, route, and socket state) stay in
//! `knet`, which keeps this crate free of business dependencies and keeps the
//! dependency graph acyclic: `posix/net -> knet -> knetns <- kprocess`.
#![no_std]

use core::{fmt, num::NonZeroU64};

/// Identifier of the initial network namespace.
///
/// `/proc/<pid>/ns/net` reports this value as both the symlink target and the
/// inode number, so it must stay stable and must never be assigned to a
/// freshly created namespace.
pub const INIT_NET_ID: NetNsId = NetNsId::new_unchecked(1);

/// Network namespace identifier.
///
/// Distinct namespaces always have distinct identifiers; `1` is reserved for
/// the initial namespace.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NetNsId(NonZeroU64);

impl NetNsId {
    const fn new_unchecked(id: u64) -> Self {
        Self(match NonZeroU64::new(id) {
            Some(id) => id,
            None => panic!("network namespace ids start at 1"),
        })
    }

    /// Creates an identifier from its raw representation.
    ///
    /// Returns `None` for `0`, which is never a valid namespace id.
    pub const fn from_u64(id: u64) -> Option<Self> {
        match NonZeroU64::new(id) {
            Some(id) => Some(Self(id)),
            None => None,
        }
    }

    /// Returns the raw identifier value used by `/proc/<pid>/ns/net`.
    pub const fn as_u64(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for NetNsId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.get())
    }
}

/// Resolves the network namespace of the calling context.
///
/// Provided by the process subsystem. The implementation must read only
/// per-task state that is valid in every execution context: it must not sleep,
/// must not take a sleepable lock, and must be safe to call from softirq,
/// timer, and task context alike.
#[kiface::interface]
pub trait NetNsResolver {
    /// Returns the network namespace the calling context runs in.
    ///
    /// Contexts without a process (boot, softirq, kernel threads) report
    /// [`INIT_NET_ID`] so that kernel-internal networking stays in the
    /// initial namespace.
    fn current() -> NetNsId;
}

#[cfg(unittest)]
mod tests {
    use core::fmt::Write as _;

    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn init_net_id_is_one() {
        assert_eq!(INIT_NET_ID.as_u64(), 1);
    }

    #[def_test]
    fn ids_are_ordered_and_zero_is_rejected() {
        let id = NetNsId::from_u64(42).unwrap();
        assert_eq!(id.as_u64(), 42);
        assert!(INIT_NET_ID < id);
        assert_eq!(NetNsId::from_u64(0), None);
    }

    #[def_test]
    fn ids_render_through_display() {
        // `no_std` without `alloc`: render through the core formatter only.
        let mut rendered = [0u8; 8];
        let written = {
            let mut cursor = Cursor {
                buffer: &mut rendered,
                written: 0,
            };
            write!(cursor, "{}", NetNsId::from_u64(42).unwrap()).unwrap();
            cursor.written
        };
        assert_eq!(&rendered[..written], b"42");
    }

    struct Cursor<'a> {
        buffer: &'a mut [u8],
        written: usize,
    }

    impl core::fmt::Write for Cursor<'_> {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let bytes = text.as_bytes();
            let rest = self
                .buffer
                .get_mut(self.written..)
                .ok_or(core::fmt::Error)?;
            rest.get_mut(..bytes.len())
                .ok_or(core::fmt::Error)?
                .copy_from_slice(bytes);
            self.written += bytes.len();
            Ok(())
        }
    }
}
