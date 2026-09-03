// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in worker-pool kinds and policy constants.

/// Pending executor entries retained by one built-in system pool.
pub(crate) const BUILTIN_POOL_ENTRY_CAP: usize = kbuild_config::WORKQUEUE_PENDING_CAP;

/// Built-in worker-pool execution kind selected by system queues.
///
/// The kind names execution context and policy template. It is not a logical
/// workqueue name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BuiltinPoolKind {
    /// Sleepable task-context worker pool.
    Normal,
    /// Bottom-half execution pool. It has no ktask-created worker lifecycle.
    Bh,
}

impl BuiltinPoolKind {
    /// All built-in pool kinds supported by the first-stage model.
    pub(crate) const ALL: [Self; Self::COUNT] = [Self::Normal, Self::Bh];
    /// Number of built-in pool kinds.
    pub(crate) const COUNT: usize = 2;

    /// Stable kind index for static per-CPU pool tables.
    pub(crate) const fn as_usize(self) -> usize {
        match self {
            Self::Normal => 0,
            Self::Bh => 1,
        }
    }

    /// Runtime-visible worker-pool kind name.
    pub(crate) const fn pool_name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Bh => "bh",
        }
    }
}
