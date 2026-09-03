// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Worker-slot state and runtime callback traits.

use ktime_types::MonotonicInstant;

use crate::{ActionBatch, RunnableClaimer, WorkerExecutionToken, WorkerId, WorkerState};

/// Current-execution concurrency accounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecutionAccounting {
    /// A running worker counts toward pool concurrency.
    Normal,
    /// A long-running callback no longer counts toward pool concurrency.
    CpuIntensive,
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

/// Runtime adapter used by the pool-owned worker task loop.
///
/// The worker pool owns the worker thread's scheduling loop: park decisions,
/// runnable FIFO popping, execution-token creation, stale-entry retry, and
/// completion accounting. This trait supplies only the environment-specific
/// pieces that the core pool cannot know. `P` is a copyable pool handle, not a
/// second pool owner:
///
/// - current time;
/// - how a worker blocks until its pool-local wake source is signalled;
/// - how returned wake/timer actions are applied after the pool lock is
///   dropped;
/// - how an accepted opaque entry is executed.
///
/// Implementors also provide [`RunnableClaimer`]. The worker loop calls the
/// claim hook after dropping the pool lock, then commits or discards the
/// reserved candidate back in the pool. Runtime methods are called only at
/// integration boundaries: waiting, applying returned [`ActionBatch`] values,
/// and running accepted product-layer work.
pub trait WorkerRuntime<P>: RunnableClaimer
where
    P: Copy,
{
    /// Returns the current monotonic time used for worker-pool accounting.
    fn now(&self) -> MonotonicInstant;

    /// Blocks until this worker should retry the pool-owned park boundary.
    fn wait_for_worker_work(&mut self, pool: P, worker: WorkerId);

    /// Applies actions returned by pool state transitions.
    ///
    /// The worker-pool core computes actions while holding the pool lock, but
    /// calls this hook only after that lock has been dropped.
    fn handle_worker_actions(&mut self, pool: P, actions: ActionBatch);

    /// Runs one claimed entry after it has been removed from the pool runnable
    /// FIFO and marked as the worker's current execution.
    fn run_claimed_work(
        &mut self,
        pool: P,
        worker: WorkerId,
        token: WorkerExecutionToken,
        work: Self::Accepted,
    );
}

/// One worker slot owned by a worker pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Worker<ThreadRef> {
    pub(crate) state: WorkerState,
    generation: WorkerExecutionToken,
    pub(crate) current: Option<CurrentExecution>,
    pub(crate) idle_since: Option<MonotonicInstant>,
    pub(crate) thread_ref: Option<ThreadRef>,
}

impl<ThreadRef> Worker<ThreadRef> {
    pub(crate) const fn new() -> Self {
        Self {
            state: WorkerState::Empty,
            generation: WorkerExecutionToken::FIRST,
            current: None,
            idle_since: None,
            thread_ref: None,
        }
    }

    pub(crate) fn set_thread_ref(&mut self, thread_ref: ThreadRef) {
        self.thread_ref = Some(thread_ref);
    }

    pub(crate) fn clear_thread_ref(&mut self) -> Option<ThreadRef> {
        self.thread_ref.take()
    }

    pub(crate) fn reserve_execution_token(&mut self) -> WorkerExecutionToken {
        self.generation = self.generation.next_generation();
        self.generation
    }

    pub(crate) fn reserved_token_matches(&self, token: WorkerExecutionToken) -> bool {
        self.generation == token
    }

    pub(crate) fn start_reserved_execution(
        &mut self,
        token: WorkerExecutionToken,
        started_at: MonotonicInstant,
    ) -> Option<CurrentExecution> {
        if self.generation != token {
            return None;
        }
        let current = CurrentExecution::new(token, started_at);
        self.current = Some(current);
        self.idle_since = None;
        Some(current)
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
