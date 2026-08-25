// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;

/// Built-in worker-pool class.
///
/// The kind names execution context and policy template. It is not a logical
/// workqueue name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinPoolKind {
    /// Sleepable task-context worker pool.
    Normal,
    /// Bottom-half execution pool. It has no provider-created worker lifecycle.
    Bh,
}

impl BuiltinPoolKind {
    /// All built-in pool kinds supported by the first-stage model.
    pub const ALL: [Self; Self::COUNT] = [Self::Normal, Self::Bh];
    /// Number of built-in pool kinds.
    pub const COUNT: usize = 2;

    /// Stable kind index for static per-CPU pool tables.
    pub const fn as_usize(self) -> usize {
        match self {
            Self::Normal => 0,
            Self::Bh => 1,
        }
    }

    /// Returns whether a per-CPU manager should process lifecycle actions.
    pub const fn is_manager_managed(self) -> bool {
        match self {
            Self::Normal => true,
            // Bottom-half pools do not own provider-created worker tasks. Their
            // wake path raises the bottom-half drain context directly, so the
            // manager has no spawn or retire slow path to execute.
            Self::Bh => false,
        }
    }
}

/// Identifier of one built-in per-CPU worker-pool instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PoolId {
    kind: BuiltinPoolKind,
    cpu: LogicalCpuId,
}

impl PoolId {
    /// Creates a built-in per-CPU pool identifier.
    pub const fn new(kind: BuiltinPoolKind, cpu: LogicalCpuId) -> Self {
        Self { kind, cpu }
    }

    /// Returns the pool kind.
    pub const fn kind(self) -> BuiltinPoolKind {
        self.kind
    }

    /// Returns the CPU that owns this per-CPU pool.
    pub const fn cpu(self) -> LogicalCpuId {
        self.cpu
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
