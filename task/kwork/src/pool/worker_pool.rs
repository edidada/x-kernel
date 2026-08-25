// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kcpu_id_map::LogicalCpuId;
use kspin::SpinNoIrq;
use ktime_types::{MonotonicInstant, TimeSpan};
use kworkerpool::{
    ActionBatch, BuiltinPoolKind, ClaimFailedResult, DispatchResult, ImmediateAction,
    ManagementAction, ManagementComplete, ParkDecision, PoolEntry, PoolId,
    QueueRemoveResult as PoolQueueRemoveResult, WorkerPool as CoreWorkerPool, WorkerPoolPolicy,
};

use super::{
    RunningWorkSlot, WorkerExecutionToken, WorkerId, WorkerSleepTransition, WorkerState,
    WorkerWakePlan,
};
use crate::{
    BarrierAttachResult, MAX_SYSTEM_WORKERS_PER_CPU, MAX_WORKQUEUE_PENDING, PendingBarrierAttach,
    QueueOwner, RunQueueEntryClaim, ScheduledWork, WorkBarrier, WorkColor, WorkEntry,
    WorkInstanceId, WorkQueuePoolBinding, WorkQueuePoolState, WorkStatus, WorkqueueTimerIf,
};

/// Default backoff after provider-side dynamic worker creation fails.
///
/// Linux uses timer-based retry/mayday paths instead of retrying worker
/// creation in a tight loop. X-Kernel keeps the same pressure-control
/// property while the task provider owns actual worker task creation.
pub const WORKER_CREATE_RETRY_DELAY: TimeSpan = TimeSpan::from_millis(10);

type WorkerPoolCore =
    CoreWorkerPool<usize, usize, WorkEntry, MAX_SYSTEM_WORKERS_PER_CPU, MAX_WORKQUEUE_PENDING>;

const DEFAULT_CORE_POOL_ID: PoolId = PoolId::new(BuiltinPoolKind::Normal, LogicalCpuId::new(0));

/// Execution domain backed by a worker pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerPoolExecution {
    /// Sleepable task-context workers created by the task provider.
    Task,
    /// Bottom-half drain context without provider-owned worker tasks.
    BottomHalf,
}

/// Scheduling class for workers or drain contexts attached to a pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerPoolSchedulingPolicy {
    /// Normal-priority execution.
    Normal,
    /// High-priority execution.
    HighPriority,
}

/// CPU placement policy for a worker pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerPoolCpuAffinity {
    /// Workers or drain context are pinned to one logical CPU.
    Pinned(LogicalCpuId),
}

/// Execution attributes shared by all work drained from one worker pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolAttrs {
    execution: WorkerPoolExecution,
    scheduling: WorkerPoolSchedulingPolicy,
    cpu_affinity: WorkerPoolCpuAffinity,
}

impl WorkerPoolAttrs {
    /// Creates worker-pool execution attributes.
    pub const fn new(
        execution: WorkerPoolExecution,
        scheduling: WorkerPoolSchedulingPolicy,
        cpu_affinity: WorkerPoolCpuAffinity,
    ) -> Self {
        Self {
            execution,
            scheduling,
            cpu_affinity,
        }
    }

    /// Returns the execution domain.
    pub const fn execution(self) -> WorkerPoolExecution {
        self.execution
    }

    /// Returns the scheduling policy.
    pub const fn scheduling(self) -> WorkerPoolSchedulingPolicy {
        self.scheduling
    }

    /// Returns the CPU affinity policy.
    pub const fn cpu_affinity(self) -> WorkerPoolCpuAffinity {
        self.cpu_affinity
    }
}

pub(crate) struct WorkerPoolPendingRemoval {
    entry: Option<WorkEntry>,
    was_runnable: bool,
}

impl WorkerPoolPendingRemoval {
    pub(crate) fn entry(&self) -> Option<&WorkEntry> {
        self.entry.as_ref()
    }

    pub(crate) fn entry_mut(&mut self) -> Option<&mut WorkEntry> {
        self.entry.as_mut()
    }

    pub(crate) fn is_runnable(&self) -> bool {
        self.entry.is_some() && self.was_runnable
    }

    pub(crate) fn into_work(self) -> Option<ScheduledWork> {
        self.entry.map(WorkEntry::into_work)
    }
}

pub(crate) struct WorkerPoolRunnableTake {
    pub(crate) work: Option<ScheduledWork>,
    pub(crate) binding: Option<WorkQueuePoolBinding>,
    pub(crate) worker_token: Option<WorkerExecutionToken>,
    pub(crate) completed_barriers: alloc::vec::Vec<WorkBarrier>,
    pub(crate) stale_entries: alloc::vec::Vec<WorkerPoolStaleEntry>,
}

pub(crate) struct WorkerPoolStaleEntry {
    pub(crate) owner: QueueOwner,
    pub(crate) status: WorkStatus,
    pub(crate) color: WorkColor,
    pub(crate) barrier_count: usize,
}

impl WorkerPoolRunnableTake {
    fn none() -> Self {
        Self {
            work: None,
            binding: None,
            worker_token: None,
            completed_barriers: alloc::vec::Vec::new(),
            stale_entries: alloc::vec::Vec::new(),
        }
    }

    fn with_work(
        mut self,
        work: ScheduledWork,
        binding: WorkQueuePoolBinding,
        worker_token: WorkerExecutionToken,
    ) -> Self {
        self.work = Some(work);
        self.binding = Some(binding);
        self.worker_token = Some(worker_token);
        self
    }
}

pub(crate) struct WorkerPool {
    attrs: SpinNoIrq<Option<WorkerPoolAttrs>>,
    stats: WorkerPoolStats,
    pub(crate) state: SpinNoIrq<WorkerPoolState>,
}

impl WorkerPool {
    pub(crate) const fn new() -> Self {
        Self {
            attrs: SpinNoIrq::new(None),
            stats: WorkerPoolStats::new(),
            state: SpinNoIrq::new(WorkerPoolState::new()),
        }
    }

    pub(crate) fn ensure_attrs(&self, attrs: WorkerPoolAttrs) {
        let mut current = self.attrs.lock();
        match *current {
            Some(existing) if existing != attrs => {
                warn!("worker pool attribute mismatch: existing={existing:?}, requested={attrs:?}");
            }
            Some(_) => {}
            None => {
                *current = Some(attrs);
                self.state.lock().configure_core(attrs);
            }
        }
    }

    pub fn attrs(&self) -> Option<WorkerPoolAttrs> {
        *self.attrs.lock()
    }

    pub(crate) fn key(&self) -> usize {
        core::ptr::from_ref(self).addr()
    }

    pub(crate) fn note_progress(&self) {
        self.stats.note_progress(WorkqueueTimerIf::monotonic_time());
    }

    pub(crate) fn update_runnable_stats(&self, state: &WorkerPoolState) {
        self.stats
            .update_runnable(state.runnable_count(), WorkqueueTimerIf::monotonic_time());
    }

    pub(crate) fn stats_snapshot(&self) -> WorkerPoolStatsSnapshot {
        self.stats.snapshot()
    }
}

pub(crate) struct WorkerPoolStatsSnapshot {
    pub(crate) runnable_count: usize,
    pub(crate) runnable_since: Option<MonotonicInstant>,
    pub(crate) last_progress: Option<MonotonicInstant>,
}

struct WorkerPoolStats {
    runnable_count: AtomicUsize,
    runnable_since_ns: AtomicU64,
    last_progress_ns: AtomicU64,
}

impl WorkerPoolStats {
    const fn new() -> Self {
        Self {
            runnable_count: AtomicUsize::new(0),
            runnable_since_ns: AtomicU64::new(0),
            last_progress_ns: AtomicU64::new(0),
        }
    }

    fn note_progress(&self, now: MonotonicInstant) {
        self.last_progress_ns
            .store(encode_instant_ns(now), Ordering::Release);
    }

    fn update_runnable(&self, runnable_count: usize, now: MonotonicInstant) {
        self.runnable_count.store(runnable_count, Ordering::Release);
        if runnable_count == 0 {
            self.runnable_since_ns.store(0, Ordering::Release);
            return;
        }

        let _ = self.runnable_since_ns.compare_exchange(
            0,
            encode_instant_ns(now),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn snapshot(&self) -> WorkerPoolStatsSnapshot {
        WorkerPoolStatsSnapshot {
            runnable_count: self.runnable_count.load(Ordering::Acquire),
            runnable_since: decode_instant_ns(self.runnable_since_ns.load(Ordering::Acquire)),
            last_progress: decode_instant_ns(self.last_progress_ns.load(Ordering::Acquire)),
        }
    }
}

fn encode_instant_ns(now: MonotonicInstant) -> u64 {
    now.as_nanos_u64_saturating().max(1)
}

fn decode_instant_ns(nanos: u64) -> Option<MonotonicInstant> {
    (nanos != 0).then(|| MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(nanos)))
}

/// Test-only state of one pool wake freeze.
///
/// Kernel preemption lets live system workers preempt a unit test and drain
/// the shared per-CPU pool mid-test. A frozen pool gives exclusive control to
/// the test: wake plans are buffered instead of executed, workers already in
/// a wait loop re-block, and provider-side drain loops stop (see
/// `SystemPoolBinding::freeze_wakes_for_tests`).
#[cfg(unittest)]
pub(crate) struct PoolTestFreeze {
    /// Wake plans buffered while frozen.
    deferred: WorkerWakePlan,
}

pub(crate) struct WorkerPoolState {
    core: WorkerPoolCore,
    pending_actions: ActionBatch,
    running: [RunningWorkSlot; MAX_SYSTEM_WORKERS_PER_CPU],
    /// Active test wake freeze, if any.
    #[cfg(unittest)]
    test_freeze: Option<PoolTestFreeze>,
}

impl WorkerPoolState {
    const fn new() -> Self {
        Self {
            core: WorkerPoolCore::new(
                DEFAULT_CORE_POOL_ID,
                WorkerPoolPolicy::for_kind(BuiltinPoolKind::Normal),
            ),
            pending_actions: ActionBatch::new(),
            running: [const { RunningWorkSlot::new() }; MAX_SYSTEM_WORKERS_PER_CPU],
            #[cfg(unittest)]
            test_freeze: None,
        }
    }

    fn configure_core(&mut self, attrs: WorkerPoolAttrs) {
        let WorkerPoolCpuAffinity::Pinned(cpu_id) = attrs.cpu_affinity();
        let kind = match attrs.execution() {
            WorkerPoolExecution::Task => BuiltinPoolKind::Normal,
            WorkerPoolExecution::BottomHalf => BuiltinPoolKind::Bh,
        };
        let _ = self
            .core
            .configure_empty(PoolId::new(kind, cpu_id), WorkerPoolPolicy::for_kind(kind));
    }

    fn now() -> MonotonicInstant {
        WorkqueueTimerIf::monotonic_time()
    }

    fn wake_plan_from_actions(actions: ActionBatch) -> WorkerWakePlan {
        let mut plan = WorkerWakePlan::default();
        for action in actions.immediate() {
            match action {
                ImmediateAction::WakeWorker { worker, .. } => {
                    if plan.worker_to_wake.is_none() {
                        plan.worker_to_wake = Some(worker);
                    }
                }
                ImmediateAction::WakeManager { .. } => {
                    plan.should_wake_manager = true;
                }
                ImmediateAction::RaiseBottomHalf { .. }
                | ImmediateAction::ArmWorkerTimer { .. } => {}
            }
        }
        for action in actions.management() {
            match action {
                ManagementAction::SpawnWorker { .. } | ManagementAction::RetireWorker { .. } => {
                    plan.should_wake_manager = true;
                }
            }
        }
        plan
    }

    fn evaluate_wake_plan(&mut self) -> WorkerWakePlan {
        let mut actions = core::mem::take(&mut self.pending_actions);
        actions.append(self.core.reevaluate_dispatch(Self::now()));
        Self::wake_plan_from_actions(actions)
    }

    fn defer_actions(&mut self, actions: ActionBatch) {
        self.pending_actions.append(actions);
    }

    pub(crate) fn runnable_count(&self) -> usize {
        self.core.runnable_len()
    }

    pub(crate) fn nr_running(&self) -> usize {
        self.core.nr_concurrency()
    }

    pub(crate) fn worker_state(&self, worker_id: usize) -> Option<WorkerState> {
        self.core.worker_state(WorkerId::new(worker_id))
    }

    pub(crate) fn worker_is_cpu_intensive(&self, worker_id: usize) -> bool {
        self.core.worker_is_cpu_intensive(WorkerId::new(worker_id))
    }

    #[cfg(unittest)]
    pub(crate) fn enqueue_test_work(
        &mut self,
        work: &ScheduledWork,
        owner: QueueOwner,
        color: WorkColor,
    ) -> Result<(), ()> {
        self.push_runnable_entry(WorkEntry::new(
            work.clone(),
            owner,
            color,
            WorkInstanceId::for_tests(1),
        ))
    }

    #[cfg(unittest)]
    pub(crate) fn set_cpu_intensive_threshold_for_tests(&mut self, threshold: TimeSpan) {
        self.core.set_cpu_intensive_threshold_for_tests(threshold);
    }

    #[cfg(unittest)]
    pub(crate) fn set_worker_create_retry_delay_for_tests(&mut self, delay: TimeSpan) {
        self.core
            .set_create_retry_delay_for_tests(delay, WorkqueueTimerIf::monotonic_time());
    }

    /// Test-only: starts a wake freeze for exclusive pool control.
    ///
    /// A second freeze while one is active keeps the first freeze state, so
    /// nested test helpers cannot clobber the saved `manager_needed` value.
    #[cfg(unittest)]
    pub(crate) fn freeze_wakes_for_tests(&mut self) {
        if self.test_freeze.is_some() {
            return;
        }
        self.test_freeze = Some(PoolTestFreeze {
            deferred: WorkerWakePlan::default(),
        });
    }

    /// Test-only: returns whether a wake freeze is active.
    #[cfg(unittest)]
    pub(crate) fn is_wake_frozen_for_tests(&self) -> bool {
        self.test_freeze.is_some()
    }

    /// Test-only: buffers `plan` in the active freeze instead of waking.
    ///
    /// Returns `true` when the pool is frozen and the plan was deferred. At
    /// most one idle worker can be kicked while frozen (a pending `Preparing`
    /// slot keeps [`Self::need_more_worker`] false), so merging keeps the
    /// first selected worker and ORs manager requests.
    #[cfg(unittest)]
    pub(crate) fn defer_wake_plan_for_tests(&mut self, plan: WorkerWakePlan) -> bool {
        let Some(freeze) = self.test_freeze.as_mut() else {
            return false;
        };
        if freeze.deferred.worker_to_wake.is_none() {
            freeze.deferred.worker_to_wake = plan.worker_to_wake;
        }
        freeze.deferred.should_wake_manager |= plan.should_wake_manager;
        true
    }

    /// Test-only: ends the wake freeze and returns the wake plan to execute.
    ///
    /// With `discard_deferred`, buffered wakes are dropped: `Preparing` slots
    /// flipped by buffered kick evaluations return to `Idle` and the saved
    /// `manager_needed` value is restored, leaving the pool quiescent for
    /// tests that drain work manually. Otherwise the merged deferred plan is
    /// returned so the caller can wake the pool and let live workers drain
    /// the work queued during the freeze.
    #[cfg(unittest)]
    pub(crate) fn unfreeze_wakes_for_tests(&mut self, discard_deferred: bool) -> WorkerWakePlan {
        let Some(freeze) = self.test_freeze.take() else {
            return WorkerWakePlan::default();
        };
        if !discard_deferred {
            return freeze.deferred;
        }
        self.core.cancel_preparing_for_tests();
        for worker_id in 0..MAX_SYSTEM_WORKERS_PER_CPU {
            let _ = self.running[worker_id].clear_current_work();
        }
        WorkerWakePlan::default()
    }

    pub(crate) fn has_runnable_work(&self) -> bool {
        self.core.runnable_len() != 0
    }

    #[cfg(unittest)]
    pub(crate) fn pending_len_for_binding(&self, binding_key: usize) -> usize {
        self.core.queued_len_for_owner(&binding_key)
    }

    #[cfg(unittest)]
    pub(crate) fn runnable_len_for_binding(&self, binding_key: usize) -> usize {
        self.core.runnable_len_for_owner(&binding_key)
    }

    #[cfg(unittest)]
    pub(crate) fn running_work_key_for_tests(&self, worker_id: usize) -> usize {
        self.running
            .get(worker_id)
            .map_or(0, RunningWorkSlot::current_work_key_for_tests)
    }

    pub(crate) fn configure_binding_max_active(
        &mut self,
        binding: &mut WorkQueuePoolState,
        binding_key: usize,
        max_active: usize,
    ) {
        binding.configure_max_active(max_active);
        let mut runnable_budget = binding.reset_active_to_running();
        let _deactivated = self.core.defer_runnable_for_owner(&binding_key);

        while runnable_budget != 0 {
            let (promoted, actions) = self.core.promote_deferred(&binding_key, 1, Self::now());
            self.defer_actions(actions);
            if promoted == 0 {
                break;
            }
            binding.add_active();
            runnable_budget -= 1;
        }
    }

    pub(crate) fn manager_should_run(&self) -> bool {
        self.core.manager_should_run(Self::now())
    }

    pub(crate) fn installed_worker_count(&self) -> usize {
        self.core.installed_workers()
    }

    pub(crate) fn select_worker_to_kick(&mut self) -> WorkerWakePlan {
        self.evaluate_wake_plan()
    }

    pub(crate) fn push_runnable_entry(&mut self, entry: WorkEntry) -> Result<(), ()> {
        let owner = entry.binding_key();
        let key = entry.work().key();
        let actions = self
            .core
            .enqueue_runnable(PoolEntry::new(owner, key, entry), Self::now())
            .map_err(|_| ())?;
        self.defer_actions(actions);
        Ok(())
    }

    pub(crate) fn push_inactive_entry(&mut self, entry: WorkEntry) -> Result<(), ()> {
        let owner = entry.binding_key();
        let key = entry.work().key();
        self.core
            .enqueue_deferred(PoolEntry::new(owner, key, entry))
            .map_err(|_| ())
    }

    pub(crate) fn remove_pending_work(
        &mut self,
        work: &ScheduledWork,
        binding_key: usize,
    ) -> WorkerPoolPendingRemoval {
        let removed = self.core.remove_entry(&binding_key, &work.key());
        let was_runnable = removed
            .as_ref()
            .is_some_and(|(_, lane)| *lane == PoolQueueRemoveResult::Runnable);
        WorkerPoolPendingRemoval {
            entry: removed.map(|(entry, _)| entry.into_payload()),
            was_runnable,
        }
    }

    pub(crate) fn remove_active_entry_locked(
        &mut self,
        binding: &mut WorkQueuePoolState,
        removed: &WorkerPoolPendingRemoval,
    ) -> bool {
        if !removed.is_runnable() {
            return false;
        }

        let Some(entry) = removed.entry() else {
            return false;
        };
        self.discard_active_entry_locked(binding, entry.binding_key())
    }

    pub(crate) fn discard_active_entry_locked(
        &mut self,
        binding: &mut WorkQueuePoolState,
        binding_key: usize,
    ) -> bool {
        binding.remove_active();
        self.activate_next_inactive_locked(binding, binding_key)
    }

    pub(crate) fn finish_active_entry_locked(
        &mut self,
        binding: &mut WorkQueuePoolState,
        binding_key: usize,
    ) -> bool {
        binding.finish_active_work();
        self.activate_next_inactive_locked(binding, binding_key)
    }

    fn activate_next_inactive_locked(
        &mut self,
        binding: &mut WorkQueuePoolState,
        binding_key: usize,
    ) -> bool {
        if !binding.can_activate() {
            return false;
        }

        let (promoted, actions) = self.core.promote_deferred(&binding_key, 1, Self::now());
        self.defer_actions(actions);
        if promoted != 0 {
            binding.add_active();
            return true;
        }
        false
    }

    pub(crate) fn attach_pending_barrier(
        &mut self,
        work: &ScheduledWork,
        binding_key: usize,
        barrier: WorkBarrier,
    ) -> PendingBarrierAttach {
        let Some(entry) = self.core.get_payload_mut(&binding_key, &work.key()) else {
            return PendingBarrierAttach::Missing;
        };
        match entry.attach_barrier(barrier) {
            BarrierAttachResult::Attached => PendingBarrierAttach::Attached,
            BarrierAttachResult::Full => PendingBarrierAttach::Full,
        }
    }

    pub(crate) fn take_any_runnable_work(
        &mut self,
        pool_key: usize,
        worker_id: WorkerId,
    ) -> WorkerPoolRunnableTake {
        let worker_index = worker_id.as_usize();
        if worker_index >= MAX_SYSTEM_WORKERS_PER_CPU {
            return WorkerPoolRunnableTake::none();
        }
        if self.running[worker_index].has_current_work() {
            return WorkerPoolRunnableTake::none();
        }
        if self.core.runnable_len() == 0 {
            self.clear_worker_current_work(worker_index);
            return WorkerPoolRunnableTake::none();
        }
        #[cfg(unittest)]
        if self.test_freeze.is_some() {
            let _ = self.core.prepare_worker_for_tests(worker_id);
        }
        if self.core.worker_state(worker_id) == Some(WorkerState::Idle) {
            let actions = self.core.reevaluate_dispatch(Self::now());
            self.defer_actions(actions);
        }

        let mut outcome = WorkerPoolRunnableTake::none();
        loop {
            let mut entry = match self.core.pop_runnable(worker_id) {
                Ok(DispatchResult::Entry(entry)) => entry.into_payload(),
                Ok(DispatchResult::Empty) => {
                    self.clear_worker_current_work(worker_index);
                    return outcome;
                }
                Err(_) => return outcome,
            };
            let binding_key = entry.binding_key();
            let instance_id = entry.instance_id();
            let entry_owner = entry.owner().clone();
            let work = entry.work().clone();
            let Ok(worker_token) = self.core.reserve_execution_token(worker_id) else {
                return outcome;
            };

            let decision = work.claim_pool_entry_for_run(
                pool_key,
                binding_key,
                instance_id,
                worker_id,
                worker_token,
            );
            match decision {
                RunQueueEntryClaim::Run {
                    binding,
                    instance_id,
                } => {
                    let linked_barriers = entry.take_barriers();
                    let actions = if let Ok(actions) =
                        self.core
                            .worker_started_with_token(worker_id, worker_token, Self::now())
                    {
                        actions
                    } else {
                        return outcome;
                    };
                    self.defer_actions(actions);
                    self.running[worker_index].set_current_work(
                        work.key(),
                        instance_id,
                        linked_barriers,
                    );
                    return outcome.with_work(work, binding, worker_token);
                }
                RunQueueEntryClaim::Stale(status) => {
                    let color = entry.color();
                    let barrier_count = entry.barrier_count();
                    outcome
                        .completed_barriers
                        .append(&mut entry.take_barriers());
                    outcome.stale_entries.push(WorkerPoolStaleEntry {
                        owner: entry_owner,
                        status,
                        color,
                        barrier_count,
                    });
                    match self.core.worker_claim_failed(worker_id, Self::now()) {
                        Ok((ClaimFailedResult::Retry, actions)) => self.defer_actions(actions),
                        Ok((ClaimFailedResult::Parked, actions)) => {
                            self.defer_actions(actions);
                            return outcome;
                        }
                        Err(_) => return outcome,
                    }
                }
            }
        }
    }

    pub(crate) fn install_worker(&mut self, worker_id: usize) -> bool {
        self.core.install_worker(WorkerId::new(worker_id)).is_ok()
    }

    #[cfg(unittest)]
    pub(crate) fn start_running_work(
        &mut self,
        worker_id: usize,
        work_key: usize,
        instance_id: WorkInstanceId,
        barriers: alloc::vec::Vec<WorkBarrier>,
    ) -> WorkerExecutionToken {
        let worker_token = self
            .core
            .start_worker_for_tests(WorkerId::new(worker_id), Self::now())
            .unwrap_or_else(|_| WorkerExecutionToken::first());
        self.running[worker_id].set_current_work(work_key, instance_id, barriers);
        worker_token
    }

    pub(crate) fn finish_running_work(
        &mut self,
        worker_id: usize,
        work_key: usize,
        instance_id: WorkInstanceId,
        worker_token: WorkerExecutionToken,
    ) -> alloc::vec::Vec<WorkBarrier> {
        if worker_id >= MAX_SYSTEM_WORKERS_PER_CPU {
            return alloc::vec::Vec::new();
        };
        if !self.running[worker_id].is_running_instance(work_key, instance_id) {
            warn!("workerqueue worker {worker_id} finished an unexpected work instance");
            return alloc::vec::Vec::new();
        }
        let actions = if let Ok(actions) =
            self.core
                .worker_finished(WorkerId::new(worker_id), worker_token, Self::now())
        {
            actions
        } else {
            warn!("workerqueue worker {worker_id} finished an unexpected work instance");
            return alloc::vec::Vec::new();
        };
        self.defer_actions(actions);
        self.running[worker_id].clear_current_work()
    }

    pub(crate) fn tick_running_worker(
        &mut self,
        worker_id: usize,
        worker_token: WorkerExecutionToken,
    ) -> WorkerWakePlan {
        Self::wake_plan_from_actions(
            self.core
                .worker_tick(WorkerId::new(worker_id), worker_token, Self::now())
                .unwrap_or_default(),
        )
    }

    pub(crate) fn worker_tick_deadline(
        &self,
        worker_id: usize,
        worker_token: WorkerExecutionToken,
    ) -> Option<MonotonicInstant> {
        self.core
            .worker_tick_deadline(WorkerId::new(worker_id), worker_token)
    }

    fn clear_worker_current_work(&mut self, worker_id: usize) {
        if worker_id < MAX_SYSTEM_WORKERS_PER_CPU {
            let _ = self.running[worker_id].clear_current_work();
        }
    }

    pub(crate) fn attach_running_barrier(
        &mut self,
        worker_id: WorkerId,
        work_key: usize,
        instance_id: WorkInstanceId,
        barrier: WorkBarrier,
    ) -> PendingBarrierAttach {
        let worker_index = worker_id.as_usize();
        if worker_index >= MAX_SYSTEM_WORKERS_PER_CPU {
            return PendingBarrierAttach::Missing;
        };
        if !self.running[worker_index].is_running_instance(work_key, instance_id) {
            return PendingBarrierAttach::Missing;
        }
        match self.running[worker_index].push_running_barrier(barrier) {
            BarrierAttachResult::Attached => PendingBarrierAttach::Attached,
            BarrierAttachResult::Full => PendingBarrierAttach::Full,
        }
    }

    pub(crate) fn mark_worker_sleeping(
        &mut self,
        worker_id: usize,
        worker_token: WorkerExecutionToken,
    ) -> WorkerSleepTransition {
        match self
            .core
            .worker_blocked(WorkerId::new(worker_id), worker_token, Self::now())
        {
            Ok(actions) => WorkerSleepTransition {
                did_sleep: true,
                wake_plan: Self::wake_plan_from_actions(actions),
            },
            Err(_) => WorkerSleepTransition {
                did_sleep: false,
                wake_plan: WorkerWakePlan::default(),
            },
        }
    }

    pub(crate) fn mark_worker_running(
        &mut self,
        worker_id: usize,
        worker_token: WorkerExecutionToken,
    ) {
        let _ = self
            .core
            .worker_resumed(WorkerId::new(worker_id), worker_token);
    }

    pub(crate) fn prepare_worker_to_wait(&mut self, worker_id: usize) -> bool {
        // A frozen pool must not hand runnable work to live workers; make
        // awake workers block in their wait loop until the test unfreezes.
        #[cfg(unittest)]
        if self.test_freeze.is_some() {
            return false;
        }
        if worker_id >= MAX_SYSTEM_WORKERS_PER_CPU {
            return false;
        }
        match self
            .core
            .worker_ready_to_park(WorkerId::new(worker_id), Self::now())
        {
            Ok((ParkDecision::Run, _)) => true,
            Ok((ParkDecision::Wait | ParkDecision::Exit, _)) | Err(_) => false,
        }
    }

    pub(crate) fn reserve_worker_creation(&mut self) -> Option<usize> {
        match self.core.next_management_action(Self::now()) {
            Some(ManagementAction::SpawnWorker { worker, .. }) => Some(worker.as_usize()),
            Some(ManagementAction::RetireWorker { .. }) | None => None,
        }
    }

    pub(crate) fn finish_worker_creation(
        &mut self,
        worker_id: usize,
        success: bool,
    ) -> WorkerWakePlan {
        let result = if success {
            ManagementComplete::Spawned
        } else {
            ManagementComplete::SpawnFailed
        };
        let actions = self
            .core
            .spawn_complete(WorkerId::new(worker_id), result, Self::now())
            .unwrap_or_default();
        if !success && worker_id < MAX_SYSTEM_WORKERS_PER_CPU {
            let _ = self.running[worker_id].clear_current_work();
        }
        Self::wake_plan_from_actions(actions)
    }
}
