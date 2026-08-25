// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::MonotonicInstant;

/// Lifecycle state of one worker slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerState {
    /// No provider task is associated with the slot.
    Empty,
    /// The manager reserved the slot and is creating a provider task.
    Creating,
    /// The provider task exists and may be selected for work.
    Idle,
    /// Idle retirement has been requested but may still be cancelled.
    RetireRequested,
    /// The provider task has begun exit and cannot be recovered.
    Exiting,
    /// A wake has been selected and the worker is on the path to taking work.
    Preparing,
    /// The worker owns a current execution.
    Running,
    /// The worker owns a current execution but is blocked in a sleepable wait.
    Sleeping,
}

/// Current-execution concurrency accounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionAccounting {
    /// A running worker counts toward pool concurrency.
    Normal,
    /// A long-running callback no longer counts toward pool concurrency.
    CpuIntensive,
}

/// Pool-local token for one worker-slot execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerExecutionToken(usize);

impl WorkerExecutionToken {
    const FIRST: Self = Self(1);

    /// First valid execution token for one worker slot.
    pub const fn first() -> Self {
        Self::FIRST
    }

    /// Returns the next token generation for the same worker slot.
    pub fn next_generation(self) -> Self {
        Self(self.0.wrapping_add(1).max(1))
    }

    /// Creates a token from a provider-owned task-local value.
    pub const fn from_usize(token: usize) -> Self {
        Self(token)
    }

    /// Returns the numeric token value.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CurrentExecution {
    token: WorkerExecutionToken,
    pub(crate) started_at: MonotonicInstant,
    accounting: ExecutionAccounting,
}

impl CurrentExecution {
    fn new(token: WorkerExecutionToken, started_at: MonotonicInstant) -> Self {
        Self {
            token,
            started_at,
            accounting: ExecutionAccounting::Normal,
        }
    }

    pub(crate) const fn token(self) -> WorkerExecutionToken {
        self.token
    }

    pub(crate) const fn accounting(self) -> ExecutionAccounting {
        self.accounting
    }
}

/// One worker slot owned by a worker pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Worker {
    pub(crate) state: WorkerState,
    generation: WorkerExecutionToken,
    pub(crate) current: Option<CurrentExecution>,
    pub(crate) idle_since: Option<MonotonicInstant>,
}

impl Worker {
    pub(crate) const fn new() -> Self {
        Self {
            state: WorkerState::Empty,
            generation: WorkerExecutionToken::FIRST,
            current: None,
            idle_since: None,
        }
    }

    pub(crate) fn next_execution(&mut self, started_at: MonotonicInstant) -> CurrentExecution {
        self.generation = self.generation.next_generation();
        let current = CurrentExecution::new(self.generation, started_at);
        self.current = Some(current);
        self.idle_since = None;
        current
    }

    pub(crate) fn current_matches(&self, token: WorkerExecutionToken) -> bool {
        self.current.is_some_and(|current| current.token == token)
    }

    pub(crate) fn mark_cpu_intensive(&mut self) {
        if let Some(current) = self.current.as_mut() {
            current.accounting = ExecutionAccounting::CpuIntensive;
        }
    }

    pub(crate) fn clear_execution(&mut self) {
        self.current = None;
        self.idle_since = None;
    }
}
