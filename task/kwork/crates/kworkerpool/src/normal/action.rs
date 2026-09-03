// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-backed runtime actions produced by worker-pool state transitions.

use ktime_types::MonotonicInstant;

use crate::WorkerId;

/// Fast-path runtime side effect requested by a pool state transition.
///
/// Normal worker-pool methods return these actions instead of performing
/// wakeups or timer operations while the pool lock is held. The runtime must
/// apply them after releasing the pool lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImmediateAction {
    /// Wake an existing runtime worker.
    WakeWorker { worker: WorkerId },
    /// Arm the CPU-intensive accounting deadline for a running worker.
    ArmCpuIntensiveTimer { deadline: MonotonicInstant },
    /// Wake the CPU-local worker-pool manager.
    WakeManager,
}

/// Slow-path lifecycle action owned by the per-CPU manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagementAction {
    /// Spawn a runtime worker for a reserved slot.
    SpawnWorker { worker: WorkerId },
    /// Wake a retiring worker so it can exit in its own context.
    RetireWorker { worker: WorkerId },
}

/// Bounded set of runtime side effects returned by normal pool transitions.
///
/// A batch is data only. It records what the runtime should do after unlocking
/// the pool; it does not wake workers, wake managers, or arm timers by itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionBatch {
    immediate: [Option<ImmediateAction>; Self::MAX_IMMEDIATE],
}

impl ActionBatch {
    const MAX_IMMEDIATE: usize = 4;

    /// Creates an empty action batch.
    pub const fn new() -> Self {
        Self {
            immediate: [const { None }; Self::MAX_IMMEDIATE],
        }
    }

    pub(crate) fn push_immediate(&mut self, action: ImmediateAction) {
        if let Some(slot) = self.immediate.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(action);
        }
    }

    /// Appends actions from another batch until this fixed batch is full.
    pub(crate) fn append(&mut self, other: Self) {
        for action in other.immediate.into_iter().flatten() {
            self.push_immediate(action);
        }
    }

    /// Iterates over immediate actions.
    pub fn immediate(&self) -> impl Iterator<Item = ImmediateAction> + '_ {
        self.immediate.iter().copied().flatten()
    }
}

impl Default for ActionBatch {
    fn default() -> Self {
        Self::new()
    }
}
