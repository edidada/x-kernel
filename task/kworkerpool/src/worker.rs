// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{MonotonicInstant, TimeSpan};

use crate::WorkerId;

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

/// Compatibility worker execution record for runtimes that keep queue and
/// provider sidecars outside [`crate::WorkerPool`].
///
/// This is a scheduling/accounting record only. It intentionally stores no
/// workqueue identity, flush barrier, or provider-task handle.
pub struct WorkerRecord {
    /// Lifecycle state of the worker slot.
    pub state: WorkerState,
    /// Current execution generation.
    pub generation: WorkerExecutionToken,
    /// Whether the current execution stopped counting toward concurrency.
    pub cpu_intensive: bool,
    /// Monotonic time when the current execution started.
    pub current_run_started: Option<MonotonicInstant>,
}

impl WorkerRecord {
    /// Creates an empty worker record.
    pub const fn new() -> Self {
        Self {
            state: WorkerState::Empty,
            generation: WorkerExecutionToken::first(),
            cpu_intensive: false,
            current_run_started: None,
        }
    }

    /// Advances and returns this slot's next execution token.
    pub fn next_execution_token(&mut self) -> WorkerExecutionToken {
        self.generation = self.generation.next_generation();
        self.generation
    }

    /// Clears per-execution accounting state.
    pub fn clear_current_execution(&mut self) {
        self.cpu_intensive = false;
        self.current_run_started = None;
    }

    /// Starts per-execution accounting state.
    pub fn start_current_execution(&mut self, run_started: MonotonicInstant) {
        self.cpu_intensive = false;
        self.current_run_started = Some(run_started);
    }

    /// Returns whether the current execution ran for at least `threshold`.
    pub fn run_exceeds(&self, threshold: TimeSpan, now: MonotonicInstant) -> bool {
        self.current_run_started
            .is_some_and(|started| now.saturating_duration_since(started) >= threshold)
    }
}

impl Default for WorkerRecord {
    fn default() -> Self {
        Self::new()
    }
}

/// Pool-side accounting result of one worker block notification.
pub struct WorkerSleepTransition {
    /// Whether the worker really transitioned Running -> Sleeping.
    pub did_sleep: bool,
    /// Wake targets chosen by kick evaluation; empty unless `did_sleep`.
    pub wake_plan: WorkerWakePlan,
}

/// Wake targets chosen by worker-pool concurrency accounting.
///
/// The plan is computed under the pool lock during kick evaluation but must
/// only be executed after the pool lock is released.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkerWakePlan {
    /// Idle worker selected to take over queued work, if any.
    pub worker_to_wake: Option<WorkerId>,
    /// Whether the pool manager task should be woken to create a worker.
    pub should_wake_manager: bool,
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
