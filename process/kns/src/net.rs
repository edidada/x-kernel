// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network namespace handle carried by the namespace proxy.
//!
//! The authoritative network namespace of a process lives on the process
//! itself as a `knetns::NetNsId` (see `kprocess::Process::net_ns`); this
//! handle only records namespace membership inside [`crate::NsProxy`] so the
//! proxy structure stays complete.

use kcred::NamespaceId;

/// Network namespace.
///
/// Placeholder until the namespace proxy owns real network namespace state.
pub struct NetNamespace {
    id: NamespaceId,
}

impl Default for NetNamespace {
    fn default() -> Self {
        Self::new()
    }
}

impl NetNamespace {
    /// Creates a new network namespace.
    pub fn new() -> Self {
        Self {
            id: NamespaceId::new(),
        }
    }

    /// Returns the namespace ID.
    pub fn id(&self) -> NamespaceId {
        self.id
    }
}
