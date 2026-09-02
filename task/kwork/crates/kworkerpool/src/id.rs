// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pool and worker identifiers.

/// Runtime-defined worker-pool class.
///
/// This is identity only. Scheduling policy, manager use, and execution
/// context are configured on each pool instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PoolKind(usize);

impl PoolKind {
    /// Creates a pool kind from a runtime-defined value.
    pub const fn new(kind: usize) -> Self {
        Self(kind)
    }

    /// Returns the runtime-defined kind value.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

/// Identifier of one worker-pool instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PoolId {
    kind: PoolKind,
}

impl PoolId {
    /// Creates a pool identifier.
    pub const fn new(kind: PoolKind) -> Self {
        Self { kind }
    }

    /// Returns the pool kind.
    pub const fn kind(self) -> PoolKind {
        self.kind
    }
}

/// Pool-local worker slot identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerId(usize);

impl WorkerId {
    /// Creates a worker id.
    pub const fn new(id: usize) -> Self {
        Self(id)
    }

    /// Returns the slot index.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}
