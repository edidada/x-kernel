// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::MonotonicInstant;

use crate::{
    ActionBatch, ExecutionAccounting, ImmediateAction, ManagementAction, PoolEntry, PoolId,
    QueueRemoveResult, WorkerExecutionToken, WorkerId, WorkerPoolPolicy, WorkerState,
    queue::PoolRunQueue, worker::Worker,
};

/// Worker-pool operation error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerPoolError<O, K, T> {
    /// Worker id is outside the configured slot table.
    InvalidWorker,
    /// Worker state does not allow the requested transition.
    InvalidWorkerState,
    /// Queue storage is full; the entry is returned to the caller.
    QueueFull(PoolEntry<O, K, T>),
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

/// Result after a popped entry failed external claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimFailedResult {
    /// More runnable entries exist; try popping another entry.
    Retry,
    /// No runnable entries remain; the worker parked.
    Parked,
}

/// Result of dispatching one prepared worker.
#[derive(Debug, Eq, PartialEq)]
pub enum DispatchResult<O, K, T> {
    /// A runnable entry was popped. The external user must claim it before
    /// calling [`WorkerPool::worker_started`].
    Entry(PoolEntry<O, K, T>),
    /// No runnable entry was available.
    Empty,
}

/// Completion result for management slow paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagementComplete {
    /// A provider worker was created.
    Spawned,
    /// Provider worker creation failed.
    SpawnFailed,
}

/// Worker-pool instance.
///
/// This type is a pure state container. The eventual runtime integration must
/// protect it with the pool-instance lock and execute returned actions only
/// after dropping that lock.
pub struct WorkerPool<O, K, T, const MAX_WORKERS: usize, const ENTRY_CAP: usize> {
    id: PoolId,
    policy: WorkerPoolPolicy,
    workers: [Worker; MAX_WORKERS],
    counts: WorkerCounts,
    queue: PoolRunQueue<O, K, T, ENTRY_CAP>,
    management_pending: bool,
    create_retry_after: Option<MonotonicInstant>,
}

impl<O, K, T, const MAX_WORKERS: usize, const ENTRY_CAP: usize>
    WorkerPool<O, K, T, MAX_WORKERS, ENTRY_CAP>
where
    O: Clone + Eq,
    K: Eq,
{
    /// Creates an empty pool instance with `policy`.
    pub const fn new(id: PoolId, policy: WorkerPoolPolicy) -> Self {
        Self {
            id,
            policy,
            workers: [const { Worker::new() }; MAX_WORKERS],
            counts: WorkerCounts::new(),
            queue: PoolRunQueue::new(),
            management_pending: false,
            create_retry_after: None,
        }
    }

    /// Returns this pool identifier.
    pub const fn id(&self) -> PoolId {
        self.id
    }

    /// Configures the identity and policy before provider workers are
    /// installed.
    ///
    /// Runtime static pool tables often need `const` construction before their
    /// CPU-local identity is known. Reconfiguration is accepted only while the
    /// pool is completely empty.
    pub fn configure_empty(&mut self, id: PoolId, policy: WorkerPoolPolicy) -> bool {
        if self.counts != WorkerCounts::new() || !self.queue.is_empty() {
            return false;
        }
        self.id = id;
        self.policy = policy;
        true
    }

    /// Returns a worker's lifecycle state.
    pub fn worker_state(&self, worker: WorkerId) -> Option<WorkerState> {
        self.workers.get(worker.as_usize()).map(|w| w.state)
    }

    /// Returns the current runnable FIFO length.
    pub fn runnable_len(&self) -> usize {
        self.queue.runnable_len()
    }

    /// Returns total queued entries across runnable and deferred lanes.
    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// Returns runnable entries for one owner.
    pub fn runnable_len_for_owner(&self, owner: &O) -> usize {
        self.queue.runnable_len_for_owner(owner)
    }

    /// Returns queued entries for one owner across runnable and deferred lanes.
    pub fn queued_len_for_owner(&self, owner: &O) -> usize {
        self.queue.runnable_len_for_owner(owner) + self.queue.deferred_len_for_owner(owner)
    }

    /// Returns the number of installed provider workers.
    pub fn installed_workers(&self) -> usize {
        self.counts.installed_workers
    }

    /// Returns the number of workers currently counted for concurrency.
    pub fn nr_concurrency(&self) -> usize {
        self.counts.nr_concurrency
    }

    /// Installs a provider worker slot.
    pub fn install_worker(&mut self, worker: WorkerId) -> Result<(), WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        if !matches!(
            self.workers[index].state,
            WorkerState::Empty | WorkerState::Creating
        ) {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.transition(index, WorkerState::Idle);
        Ok(())
    }

    /// Enqueues an entry directly into the runnable FIFO and evaluates dispatch.
    pub fn enqueue_runnable(
        &mut self,
        entry: PoolEntry<O, K, T>,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError<O, K, T>> {
        self.queue
            .push_runnable(entry)
            .map_err(WorkerPoolError::QueueFull)?;
        Ok(self.evaluate_dispatch(now))
    }

    /// Enqueues an entry into its owner's deferred lane.
    pub fn enqueue_deferred(
        &mut self,
        entry: PoolEntry<O, K, T>,
    ) -> Result<(), WorkerPoolError<O, K, T>> {
        self.queue
            .push_deferred(entry)
            .map_err(WorkerPoolError::QueueFull)
    }

    /// Promotes up to `budget` deferred entries for `owner` into runnable FIFO.
    pub fn promote_deferred(
        &mut self,
        owner: &O,
        budget: usize,
        now: MonotonicInstant,
    ) -> (usize, ActionBatch) {
        let promoted = self.queue.promote_deferred(owner, budget);
        let actions = if promoted == 0 {
            ActionBatch::new()
        } else {
            self.evaluate_dispatch(now)
        };
        (promoted, actions)
    }

    /// Moves all runnable entries for `owner` into that owner's deferred lane.
    pub fn defer_runnable_for_owner(&mut self, owner: &O) -> usize {
        self.queue.defer_runnable_for_owner(owner)
    }

    /// Removes an entry from runnable or deferred queues.
    pub fn remove_entry(
        &mut self,
        owner: &O,
        key: &K,
    ) -> Option<(PoolEntry<O, K, T>, QueueRemoveResult)> {
        self.queue.remove(owner, key)
    }

    /// Returns a mutable payload reference by owner and key.
    pub fn get_payload_mut(&mut self, owner: &O, key: &K) -> Option<&mut T> {
        self.queue.get_mut(owner, key)
    }

    /// Pops a runnable entry for a prepared worker.
    pub fn pop_runnable(
        &mut self,
        worker: WorkerId,
    ) -> Result<DispatchResult<O, K, T>, WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Preparing {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        Ok(match self.queue.pop_runnable() {
            Some(entry) => DispatchResult::Entry(entry),
            None => DispatchResult::Empty,
        })
    }

    /// Reports that a popped entry failed external claim.
    pub fn worker_claim_failed(
        &mut self,
        worker: WorkerId,
        now: MonotonicInstant,
    ) -> Result<(ClaimFailedResult, ActionBatch), WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Preparing {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        if self.queue.runnable_len() != 0 {
            return Ok((ClaimFailedResult::Retry, ActionBatch::new()));
        }
        self.mark_idle_for_wait(index, now);
        Ok((
            ClaimFailedResult::Parked,
            self.arm_retire_timer_if_needed(index, now),
        ))
    }

    /// Marks a prepared worker as running after external claim succeeded.
    pub fn worker_started(
        &mut self,
        worker: WorkerId,
        now: MonotonicInstant,
    ) -> Result<(WorkerExecutionToken, ActionBatch), WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Preparing {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.transition(index, WorkerState::Running);
        let current = self.workers[index].next_execution(now);
        self.counts.nr_concurrency += 1;
        let mut actions = ActionBatch::new();
        if let Some(deadline) = now.checked_add(self.policy.cpu_intensive_threshold()) {
            actions.push_immediate(ImmediateAction::ArmWorkerTimer {
                pool: self.id,
                deadline,
            });
        }
        Ok((current.token(), actions))
    }

    /// Reserves an execution token for a prepared worker before external
    /// claim state is updated.
    pub fn reserve_execution_token(
        &mut self,
        worker: WorkerId,
    ) -> Result<WorkerExecutionToken, WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Preparing {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        Ok(self.workers[index].reserve_execution_token())
    }

    /// Marks a prepared worker as running with a previously reserved token.
    pub fn worker_started_with_token(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Preparing {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.transition(index, WorkerState::Running);
        if self.workers[index]
            .start_reserved_execution(token, now)
            .is_none()
        {
            self.transition(index, WorkerState::Preparing);
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.counts.nr_concurrency += 1;
        let mut actions = ActionBatch::new();
        if let Some(deadline) = now.checked_add(self.policy.cpu_intensive_threshold()) {
            actions.push_immediate(ImmediateAction::ArmWorkerTimer {
                pool: self.id,
                deadline,
            });
        }
        Ok(actions)
    }

    /// Finishes a worker execution and prepares the worker for more runnable
    /// work when possible.
    pub fn worker_finished(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        let state = self.workers[index].state;
        if !matches!(state, WorkerState::Running | WorkerState::Sleeping)
            || !self.workers[index].current_matches(token)
        {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        if state == WorkerState::Running
            && self.workers[index]
                .current
                .is_some_and(|current| current.accounting() == ExecutionAccounting::Normal)
        {
            self.counts.nr_concurrency = self.counts.nr_concurrency.saturating_sub(1);
        }
        self.workers[index].clear_execution();
        if self.queue.runnable_len() != 0 {
            self.transition(index, WorkerState::Preparing);
            let mut actions = ActionBatch::new();
            actions.push_immediate(ImmediateAction::WakeWorker {
                pool: self.id,
                worker,
            });
            Ok(actions)
        } else {
            self.mark_idle_for_wait(index, now);
            Ok(self.arm_retire_timer_if_needed(index, now))
        }
    }

    /// Reports that a running worker blocked in a sleepable wait.
    pub fn worker_blocked(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Running
            || !self.workers[index].current_matches(token)
        {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        if self.workers[index]
            .current
            .is_some_and(|current| current.accounting() == ExecutionAccounting::Normal)
        {
            self.counts.nr_concurrency = self.counts.nr_concurrency.saturating_sub(1);
        }
        self.transition(index, WorkerState::Sleeping);
        Ok(self.evaluate_dispatch(now))
    }

    /// Reports that a sleeping worker resumed.
    pub fn worker_resumed(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
    ) -> Result<(), WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Sleeping
            || !self.workers[index].current_matches(token)
        {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.transition(index, WorkerState::Running);
        if self.workers[index]
            .current
            .is_some_and(|current| current.accounting() == ExecutionAccounting::Normal)
        {
            self.counts.nr_concurrency += 1;
        }
        Ok(())
    }

    /// Accounts a worker tick and marks CPU-intensive executions.
    pub fn worker_tick(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        let Some(current) = self.workers[index].current else {
            return Err(WorkerPoolError::InvalidWorkerState);
        };
        if self.workers[index].state != WorkerState::Running
            || current.token() != token
            || current.accounting() == ExecutionAccounting::CpuIntensive
            || now.saturating_duration_since(current.started_at)
                < self.policy.cpu_intensive_threshold()
        {
            return Ok(ActionBatch::new());
        }
        self.workers[index].mark_cpu_intensive();
        self.counts.nr_concurrency = self.counts.nr_concurrency.saturating_sub(1);
        Ok(self.evaluate_dispatch(now))
    }

    /// Returns the CPU-intensive accounting deadline for a running execution.
    pub fn worker_tick_deadline(
        &self,
        worker: WorkerId,
        token: WorkerExecutionToken,
    ) -> Option<MonotonicInstant> {
        let index = self.checked_worker(worker).ok()?;
        let current = self.workers[index].current?;
        if self.workers[index].state != WorkerState::Running
            || current.token() != token
            || current.accounting() == ExecutionAccounting::CpuIntensive
        {
            return None;
        }
        current
            .started_at
            .checked_add(self.policy.cpu_intensive_threshold())
    }

    /// Returns what a worker at the wait boundary should do.
    pub fn worker_ready_to_park(
        &mut self,
        worker: WorkerId,
        now: MonotonicInstant,
    ) -> Result<(ParkDecision, ActionBatch), WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        match self.workers[index].state {
            WorkerState::RetireRequested => {
                self.transition(index, WorkerState::Exiting);
                Ok((ParkDecision::Exit, ActionBatch::new()))
            }
            WorkerState::Preparing if self.queue.runnable_len() != 0 => {
                Ok((ParkDecision::Run, ActionBatch::new()))
            }
            WorkerState::Preparing | WorkerState::Idle => {
                self.mark_idle_for_wait(index, now);
                Ok((
                    ParkDecision::Wait,
                    self.arm_retire_timer_if_needed(index, now),
                ))
            }
            _ => Err(WorkerPoolError::InvalidWorkerState),
        }
    }

    /// Completes worker exit after provider-side task cleanup.
    pub fn worker_exit_complete(
        &mut self,
        worker: WorkerId,
    ) -> Result<(), WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        if self.workers[index].state != WorkerState::Exiting {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.transition(index, WorkerState::Empty);
        self.workers[index].clear_execution();
        Ok(())
    }

    /// Retire timer expired; request manager processing if any worker is still
    /// eligible.
    pub fn retire_timer_expired(&mut self, now: MonotonicInstant) -> ActionBatch {
        if self.find_retirable_idle(now).is_some() {
            self.management_pending = true;
            let mut actions = ActionBatch::new();
            actions.push_immediate(ImmediateAction::WakeManager { pool: self.id });
            return actions;
        }
        ActionBatch::new()
    }

    /// Returns the next slow-path lifecycle action for the per-CPU manager.
    pub fn next_management_action(&mut self, now: MonotonicInstant) -> Option<ManagementAction> {
        self.management_pending = false;
        if self.can_spawn_worker(now)
            && let Some(index) = self.find_state(WorkerState::Empty)
        {
            self.transition(index, WorkerState::Creating);
            return Some(ManagementAction::SpawnWorker {
                pool: self.id,
                worker: WorkerId::new(index),
            });
        }
        if self.policy.idle_retire()
            && let Some(index) = self.find_retirable_idle(now)
        {
            self.transition(index, WorkerState::RetireRequested);
            return Some(ManagementAction::RetireWorker {
                pool: self.id,
                worker: WorkerId::new(index),
            });
        }
        None
    }

    /// Completes a spawn action.
    pub fn spawn_complete(
        &mut self,
        worker: WorkerId,
        result: ManagementComplete,
        now: MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        match result {
            ManagementComplete::Spawned => {
                match self.workers[index].state {
                    WorkerState::Creating => self.transition(index, WorkerState::Idle),
                    WorkerState::Idle => {}
                    _ => return Err(WorkerPoolError::InvalidWorkerState),
                }
                self.create_retry_after = None;
            }
            ManagementComplete::SpawnFailed => {
                if self.workers[index].state != WorkerState::Creating {
                    return Err(WorkerPoolError::InvalidWorkerState);
                }
                self.transition(index, WorkerState::Empty);
                self.create_retry_after = now.checked_add(self.policy.create_retry_delay());
            }
        }
        Ok(self.evaluate_dispatch(now))
    }

    /// Re-evaluates dispatch after external queue policy made entries runnable.
    pub fn reevaluate_dispatch(&mut self, now: MonotonicInstant) -> ActionBatch {
        self.evaluate_dispatch(now)
    }

    /// Returns whether a manager slow path should run now.
    pub fn manager_should_run(&self, now: MonotonicInstant) -> bool {
        self.management_pending
            && (self.can_spawn_worker(now)
                || (self.policy.idle_retire() && self.find_retirable_idle(now).is_some()))
    }

    /// Returns whether the current execution has CPU-intensive accounting.
    pub fn worker_is_cpu_intensive(&self, worker: WorkerId) -> bool {
        self.checked_worker(worker)
            .ok()
            .and_then(|index| self.workers[index].current)
            .is_some_and(|current| current.accounting() == ExecutionAccounting::CpuIntensive)
    }

    #[cfg(unittest)]
    pub fn set_cpu_intensive_threshold_for_tests(&mut self, threshold: ktime_types::TimeSpan) {
        self.policy.set_cpu_intensive_threshold_for_tests(threshold);
    }

    #[cfg(unittest)]
    pub fn set_create_retry_delay_for_tests(
        &mut self,
        delay: ktime_types::TimeSpan,
        now: MonotonicInstant,
    ) {
        self.policy.set_create_retry_delay_for_tests(delay);
        if self.create_retry_after.is_some() {
            self.create_retry_after = now.checked_add(delay);
        }
    }

    #[cfg(unittest)]
    pub fn cancel_preparing_for_tests(&mut self) {
        for index in 0..MAX_WORKERS {
            if self.workers[index].state == WorkerState::Preparing {
                self.transition(index, WorkerState::Idle);
                self.workers[index].clear_execution();
            }
        }
    }

    #[cfg(unittest)]
    pub fn prepare_worker_for_tests(
        &mut self,
        worker: WorkerId,
    ) -> Result<(), WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        self.cancel_preparing_for_tests();
        if self.workers[index].state != WorkerState::Idle {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.transition(index, WorkerState::Preparing);
        Ok(())
    }

    #[cfg(unittest)]
    pub fn start_worker_for_tests(
        &mut self,
        worker: WorkerId,
        now: MonotonicInstant,
    ) -> Result<WorkerExecutionToken, WorkerPoolError<O, K, T>> {
        let index = self.checked_worker(worker)?;
        match self.workers[index].state {
            WorkerState::Idle => self.transition(index, WorkerState::Preparing),
            WorkerState::Preparing => {}
            WorkerState::Running => {
                let token = self.workers[index].reserve_execution_token();
                let _ = self.workers[index].start_reserved_execution(token, now);
                return Ok(token);
            }
            _ => return Err(WorkerPoolError::InvalidWorkerState),
        }
        let token = self.workers[index].reserve_execution_token();
        self.worker_started_with_token(worker, token, now)?;
        Ok(token)
    }

    fn checked_worker(&self, worker: WorkerId) -> Result<usize, WorkerPoolError<O, K, T>> {
        let index = worker.as_usize();
        if index < MAX_WORKERS {
            Ok(index)
        } else {
            Err(WorkerPoolError::InvalidWorker)
        }
    }

    fn evaluate_dispatch(&mut self, now: MonotonicInstant) -> ActionBatch {
        let mut actions = ActionBatch::new();
        self.mark_cpu_intensive_workers(now);
        if self.queue.runnable_len() == 0
            || self.counts.nr_concurrency != 0
            || self.counts.nr_preparing != 0
        {
            return actions;
        }

        if let Some(index) = self.find_state(WorkerState::RetireRequested) {
            self.transition(index, WorkerState::Preparing);
            actions.push_immediate(ImmediateAction::WakeWorker {
                pool: self.id,
                worker: WorkerId::new(index),
            });
            return actions;
        }
        if let Some(index) = self.find_state(WorkerState::Idle) {
            self.transition(index, WorkerState::Preparing);
            actions.push_immediate(ImmediateAction::WakeWorker {
                pool: self.id,
                worker: WorkerId::new(index),
            });
            return actions;
        }
        if self.can_spawn_worker(now) {
            self.management_pending = true;
            actions.push_immediate(ImmediateAction::WakeManager { pool: self.id });
        }
        actions
    }

    fn mark_cpu_intensive_workers(&mut self, now: MonotonicInstant) {
        if self.queue.runnable_len() == 0 || self.counts.nr_concurrency == 0 {
            return;
        }
        for index in 0..MAX_WORKERS {
            if self.workers[index].state != WorkerState::Running {
                continue;
            }
            let Some(current) = self.workers[index].current else {
                continue;
            };
            if current.accounting() == ExecutionAccounting::CpuIntensive
                || now.saturating_duration_since(current.started_at)
                    < self.policy.cpu_intensive_threshold()
            {
                continue;
            }
            self.workers[index].mark_cpu_intensive();
            self.counts.nr_concurrency = self.counts.nr_concurrency.saturating_sub(1);
        }
    }

    fn can_spawn_worker(&self, now: MonotonicInstant) -> bool {
        self.policy.dynamic_create()
            && self.id.kind().is_manager_managed()
            && self.queue.runnable_len() != 0
            && self.counts.nr_concurrency == 0
            && self.counts.nr_preparing == 0
            && self.counts.nr_retire_requested == 0
            && self.counts.nr_creating == 0
            && self.counts.installed_workers + self.counts.nr_creating < self.policy.max_workers()
            && self
                .create_retry_after
                .is_none_or(|deadline| now >= deadline)
    }

    fn find_retirable_idle(&self, now: MonotonicInstant) -> Option<usize> {
        if !self.policy.idle_retire() || self.counts.installed_workers <= self.policy.min_workers()
        {
            return None;
        }
        let retire_after = self.policy.idle_retire_after()?;
        self.workers.iter().position(|worker| {
            worker.state == WorkerState::Idle
                && worker.idle_since.is_some_and(|idle_since| {
                    now.saturating_duration_since(idle_since) >= retire_after
                })
        })
    }

    fn mark_idle_for_wait(&mut self, index: usize, now: MonotonicInstant) {
        if self.workers[index].state != WorkerState::Idle {
            self.transition(index, WorkerState::Idle);
        }
        self.workers[index].idle_since = Some(now);
    }

    fn arm_retire_timer_if_needed(&self, index: usize, now: MonotonicInstant) -> ActionBatch {
        let mut actions = ActionBatch::new();
        if !self.policy.idle_retire() || self.counts.installed_workers <= self.policy.min_workers()
        {
            return actions;
        }
        let Some(retire_after) = self.policy.idle_retire_after() else {
            return actions;
        };
        if self.workers[index].idle_since.is_some()
            && let Some(deadline) = now.checked_add(retire_after)
        {
            actions.push_immediate(ImmediateAction::ArmWorkerTimer {
                pool: self.id,
                deadline,
            });
        }
        actions
    }

    fn find_state(&self, state: WorkerState) -> Option<usize> {
        self.workers.iter().position(|worker| worker.state == state)
    }

    fn transition(&mut self, index: usize, new: WorkerState) {
        let old = self.workers[index].state;
        if old == new {
            return;
        }
        self.counts.leave(old);
        self.workers[index].state = new;
        self.counts.enter(new);
        if !matches!(new, WorkerState::Idle) {
            self.workers[index].idle_since = None;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkerCounts {
    installed_workers: usize,
    nr_creating: usize,
    nr_idle: usize,
    nr_preparing: usize,
    nr_running_state: usize,
    nr_sleeping: usize,
    nr_retire_requested: usize,
    nr_exiting: usize,
    nr_concurrency: usize,
}

impl WorkerCounts {
    const fn new() -> Self {
        Self {
            installed_workers: 0,
            nr_creating: 0,
            nr_idle: 0,
            nr_preparing: 0,
            nr_running_state: 0,
            nr_sleeping: 0,
            nr_retire_requested: 0,
            nr_exiting: 0,
            nr_concurrency: 0,
        }
    }

    fn enter(&mut self, state: WorkerState) {
        match state {
            WorkerState::Empty => {}
            WorkerState::Creating => self.nr_creating += 1,
            WorkerState::Idle => {
                self.installed_workers += 1;
                self.nr_idle += 1;
            }
            WorkerState::RetireRequested => {
                self.installed_workers += 1;
                self.nr_retire_requested += 1;
            }
            WorkerState::Exiting => {
                self.installed_workers += 1;
                self.nr_exiting += 1;
            }
            WorkerState::Preparing => {
                self.installed_workers += 1;
                self.nr_preparing += 1;
            }
            WorkerState::Running => {
                self.installed_workers += 1;
                self.nr_running_state += 1;
            }
            WorkerState::Sleeping => {
                self.installed_workers += 1;
                self.nr_sleeping += 1;
            }
        }
    }

    fn leave(&mut self, state: WorkerState) {
        match state {
            WorkerState::Empty => {}
            WorkerState::Creating => self.nr_creating = self.nr_creating.saturating_sub(1),
            WorkerState::Idle => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_idle = self.nr_idle.saturating_sub(1);
            }
            WorkerState::RetireRequested => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_retire_requested = self.nr_retire_requested.saturating_sub(1);
            }
            WorkerState::Exiting => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_exiting = self.nr_exiting.saturating_sub(1);
            }
            WorkerState::Preparing => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_preparing = self.nr_preparing.saturating_sub(1);
            }
            WorkerState::Running => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_running_state = self.nr_running_state.saturating_sub(1);
            }
            WorkerState::Sleeping => {
                self.installed_workers = self.installed_workers.saturating_sub(1);
                self.nr_sleeping = self.nr_sleeping.saturating_sub(1);
            }
        }
    }
}
