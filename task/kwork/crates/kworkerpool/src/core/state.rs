// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared worker-pool state types.

use super::PoolEntry;

/// Lifecycle state of one worker slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerState {
    /// No runtime task is associated with the slot.
    Empty,
    /// The manager reserved the slot and is creating a runtime task.
    Creating,
    /// The runtime task exists and may be selected for work.
    Idle,
    /// Idle retirement has been requested but may still be cancelled.
    RetireRequested,
    /// The runtime task has begun exit and cannot be recovered.
    Exiting,
    /// A wake has been selected and the worker must re-enter the pool loop.
    ///
    /// This state is wait-ready even if the runnable queue is observed empty:
    /// the worker loop owns the transition back to `Idle`, and runtime wake
    /// delivery is allowed to be repeated while runnable work remains.
    Preparing,
    /// The worker popped an entry and waits for external claim validation.
    Claiming,
    /// The worker owns a current execution.
    Running,
    /// The worker owns a current execution but is blocked in a sleepable wait.
    Sleeping,
}

/// Pool-local token for one worker-slot execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerExecutionToken(usize);

impl WorkerExecutionToken {
    pub(crate) const FIRST: Self = Self(1);

    /// Returns the next token generation for the same worker slot.
    pub(crate) fn next_generation(self) -> Self {
        Self(self.0.wrapping_add(1).max(1))
    }

    /// Creates a token from a runtime-owned task-local value.
    pub(crate) const fn from_usize(token: usize) -> Self {
        Self(token)
    }

    /// Returns the numeric token value.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

/// Worker-pool operation error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerPoolError {
    /// Worker id is outside the configured slot table.
    InvalidWorker,
    /// Worker state does not allow the requested transition.
    InvalidWorkerState,
    /// Queue storage is full; the entry is returned to the caller.
    QueueFull(PoolEntry),
}

/// Worker park decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkDecision {
    /// Runnable entries are available; the worker should try taking one.
    Run,
    /// No runnable entries are available; the worker should block or park.
    Wait,
    /// The worker should exit in its own context.
    Exit,
}

/// Read-only worker-pool state snapshot for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolSnapshot<const MAX_WORKERS: usize> {
    pub installed_workers: usize,
    pub nr_creating: usize,
    pub nr_idle: usize,
    pub nr_preparing: usize,
    pub nr_claiming: usize,
    pub nr_running_state: usize,
    pub nr_sleeping: usize,
    pub nr_retire_requested: usize,
    pub nr_exiting: usize,
    pub nr_concurrency: usize,
    pub runnable: usize,
    pub deferred: usize,
    pub worker_states: [WorkerState; MAX_WORKERS],
}
