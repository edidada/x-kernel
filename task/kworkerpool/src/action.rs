// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::MonotonicInstant;

use crate::{PoolId, WorkerId};

/// Fast-path action that can be executed by the current runtime context after
/// the pool lock is dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImmediateAction {
    /// Wake an existing provider worker.
    WakeWorker { pool: PoolId, worker: WorkerId },
    /// Raise the bottom-half drain context for this pool.
    RaiseBottomHalf { pool: PoolId },
    /// Arm a worker accounting timer.
    ArmWorkerTimer {
        pool: PoolId,
        deadline: MonotonicInstant,
    },
    /// Wake the CPU-local worker-pool manager.
    WakeManager { pool: PoolId },
}

/// Slow-path lifecycle action owned by the per-CPU manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagementAction {
    /// Spawn a provider worker for a reserved slot.
    SpawnWorker { pool: PoolId, worker: WorkerId },
    /// Wake a retiring worker so it can exit in its own context.
    RetireWorker { pool: PoolId, worker: WorkerId },
}

/// Small fixed action batch returned by pool state transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionBatch {
    immediate: [Option<ImmediateAction>; Self::MAX_IMMEDIATE],
    management: [Option<ManagementAction>; Self::MAX_MANAGEMENT],
}

impl ActionBatch {
    const MAX_IMMEDIATE: usize = 4;
    const MAX_MANAGEMENT: usize = 2;

    /// Creates an empty action batch.
    pub const fn new() -> Self {
        Self {
            immediate: [const { None }; Self::MAX_IMMEDIATE],
            management: [const { None }; Self::MAX_MANAGEMENT],
        }
    }

    pub(crate) fn push_immediate(&mut self, action: ImmediateAction) {
        if let Some(slot) = self.immediate.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(action);
        }
    }

    /// Appends one management action if the fixed batch has capacity.
    pub fn push_management(&mut self, action: ManagementAction) {
        if let Some(slot) = self.management.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(action);
        }
    }

    /// Appends actions from another batch until this fixed batch is full.
    pub fn append(&mut self, other: Self) {
        for action in other.immediate.into_iter().flatten() {
            self.push_immediate(action);
        }
        for action in other.management.into_iter().flatten() {
            self.push_management(action);
        }
    }

    /// Iterates over immediate actions.
    pub fn immediate(&self) -> impl Iterator<Item = ImmediateAction> + '_ {
        self.immediate.iter().copied().flatten()
    }

    /// Iterates over management actions.
    pub fn management(&self) -> impl Iterator<Item = ManagementAction> + '_ {
        self.management.iter().copied().flatten()
    }

    /// Returns whether the batch has no action.
    pub fn is_empty(&self) -> bool {
        self.immediate().next().is_none() && self.management().next().is_none()
    }
}

impl Default for ActionBatch {
    fn default() -> Self {
        Self::new()
    }
}
