// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ktask-backed runtime wrapper for worker-pool state.

use alloc::{string::String, sync::Arc};
use core::{future::poll_fn, task::Poll};

use kcpu_id_map::LogicalCpuId;
use kpoll::{PollEvent, PollRegistrations};
use kspin::{SpinNoIrq, SpinNoIrqGuard};
use ktime_types::MonotonicInstant;

use super::{
    action::ManagementAction,
    execution::WorkerPoolIdentity,
    manager::{ManagerRuntime, ManagerTarget, ManagerTask},
    pool::ManagementComplete,
    task_loop::WorkerTask,
    thread::{prepare_bound_kthread, worker_name},
};
use crate::{
    ActionBatch, ImmediateAction, WorkerId, WorkerPool, WorkerPoolPolicy, WorkerRuntime,
    WorkerThreadRef, decode_task_context,
};

/// Builds the runtime object used by one ktask-backed worker.
///
/// The factory is registered on a [`KtaskWorkerPool`] before workers are
/// started. It receives only worker-pool runtime identity and this worker's
/// wait source; callback storage and workqueue semantics remain outside
/// `kworkerpool`.
pub type WorkerRuntimeFactory<R> = fn(WorkerPoolPlacement, WorkerId, Arc<PollEvent>) -> R;

struct RegisteredWorkerRuntime<R> {
    pool_name: &'static str,
    factory: WorkerRuntimeFactory<R>,
}

impl<R> Clone for RegisteredWorkerRuntime<R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R> Copy for RegisteredWorkerRuntime<R> {}

/// Runtime placement for ktask-backed worker-pool tasks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolPlacement {
    cpu_id: LogicalCpuId,
}

impl WorkerPoolPlacement {
    /// Creates a placement that binds worker and manager tasks to `cpu_id`.
    pub const fn bound_to(cpu_id: LogicalCpuId) -> Self {
        Self { cpu_id }
    }

    /// Returns the CPU where ktask-backed pool tasks should run.
    pub const fn cpu_id(self) -> LogicalCpuId {
        self.cpu_id
    }
}

/// Result of accounting one ktask execution tick for a worker-pool context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionTickResult {
    /// Runtime actions produced by CPU-intensive accounting.
    pub actions: ActionBatch,
    /// Next CPU-intensive accounting deadline, if the current execution still
    /// needs one.
    pub deadline: Option<MonotonicInstant>,
}

/// ktask-backed normal worker pool.
///
/// The core pool owns scheduling state. This wrapper owns ktask-specific
/// objects attached to that state: the registered worker runtime factory,
/// worker tasks, worker wake sources, and the manager task wake source.
pub struct KtaskWorkerPool<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize> {
    inner: SpinNoIrq<WorkerPool<WorkerThreadRef, MAX_WORKERS, ENTRY_CAP>>,
    placement: SpinNoIrq<WorkerPoolPlacement>,
    runtime: SpinNoIrq<Option<RegisteredWorkerRuntime<R>>>,
    manager_thread: SpinNoIrq<Option<WorkerThreadRef>>,
}

impl<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize>
    KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>
{
    /// Creates one ktask-backed pool instance.
    pub const fn new(placement: WorkerPoolPlacement, policy: WorkerPoolPolicy) -> Self {
        Self {
            inner: SpinNoIrq::new(WorkerPool::new(policy)),
            placement: SpinNoIrq::new(placement),
            runtime: SpinNoIrq::new(None),
            manager_thread: SpinNoIrq::new(None),
        }
    }

    /// Returns this pool instance's opaque accounting identity.
    pub(crate) fn identity(&self) -> WorkerPoolIdentity {
        WorkerPoolIdentity::new(core::ptr::from_ref(self).addr())
    }

    /// Locks the core worker-pool state.
    pub fn lock(&self) -> SpinNoIrqGuard<'_, WorkerPool<WorkerThreadRef, MAX_WORKERS, ENTRY_CAP>> {
        self.inner.lock()
    }

    /// Tries to lock the core worker-pool state without spinning.
    pub fn try_lock(
        &self,
    ) -> Option<SpinNoIrqGuard<'_, WorkerPool<WorkerThreadRef, MAX_WORKERS, ENTRY_CAP>>> {
        self.inner.try_lock()
    }

    /// Returns where this ktask-backed pool creates worker and manager tasks.
    pub fn placement(&self) -> WorkerPoolPlacement {
        *self.placement.lock()
    }

    /// Reconfigures an empty const-initialized pool.
    pub fn configure_empty(
        &self,
        placement: WorkerPoolPlacement,
        policy: WorkerPoolPolicy,
    ) -> bool {
        if self.manager_thread.lock().is_some() || self.runtime.lock().is_some() {
            return false;
        }
        let configured = self.inner.lock().configure_empty(policy);
        if !configured {
            return false;
        }
        *self.placement.lock() = placement;
        true
    }

    /// Registers how this pool creates worker runtime state.
    ///
    /// The factory is called whenever this pool creates a ktask worker for an
    /// internally selected worker slot. Register it before starting initial
    /// workers or the manager.
    pub fn register_worker_runtime(
        &self,
        pool_name: &'static str,
        factory: WorkerRuntimeFactory<R>,
    ) -> bool {
        if self.manager_thread.lock().is_some() || self.inner.lock().installed_workers() != 0 {
            return false;
        }
        let mut runtime = self.runtime.lock();
        if runtime.is_some() {
            return false;
        }
        *runtime = Some(RegisteredWorkerRuntime { pool_name, factory });
        true
    }

    /// Installs one already-created worker task into a pool slot.
    pub(crate) fn install_worker(&self, worker: WorkerId, thread_ref: WorkerThreadRef) -> bool {
        self.inner.lock().install_worker(worker, thread_ref).is_ok()
    }

    /// Installs the manager task used to drive slow-path lifecycle actions.
    pub(crate) fn install_manager_thread(&self, thread_ref: WorkerThreadRef) -> bool {
        let mut slot = self.manager_thread.lock();
        if slot.is_some() {
            return false;
        }
        *slot = Some(thread_ref);
        true
    }

    /// Returns one worker thread reference.
    pub(crate) fn worker_thread_ref(&self, worker: WorkerId) -> Option<WorkerThreadRef> {
        self.inner.lock().worker_thread_ref(worker).cloned()
    }

    fn manager_thread_ref(&self) -> Option<WorkerThreadRef> {
        self.manager_thread.lock().clone()
    }

    /// Returns whether this pool has an installed manager task.
    pub fn has_manager_thread(&self) -> bool {
        self.manager_thread.lock().is_some()
    }

    /// Wakes one worker, if the slot is installed.
    pub fn wake_worker(&self, worker: WorkerId) -> bool {
        let Some(thread_ref) = self.worker_thread_ref(worker) else {
            return false;
        };
        thread_ref.wake();
        true
    }

    /// Wakes this pool's manager task, if one is installed.
    pub fn wake_manager(&self) -> bool {
        let Some(thread_ref) = self.manager_thread_ref() else {
            return false;
        };
        thread_ref.wake();
        true
    }

    /// Accounts a ktask execution tick if `context` belongs to this pool.
    pub fn account_execution_tick(
        &self,
        context: ktask::TaskExecutionContext,
        now: MonotonicInstant,
    ) -> Option<ExecutionTickResult> {
        let parts = decode_task_context(context)?;
        if parts.pool != self.identity() || parts.cpu_id != self.placement().cpu_id() {
            return None;
        }

        let actions = self
            .inner
            .lock()
            .worker_tick(parts.worker, parts.token, now)
            .ok()?;
        let deadline = self
            .inner
            .lock()
            .worker_tick_deadline(parts.worker, parts.token);
        Some(ExecutionTickResult { actions, deadline })
    }

    /// Returns the execution tick deadline if `context` belongs to this pool.
    pub fn execution_tick_deadline(
        &self,
        context: ktask::TaskExecutionContext,
    ) -> Option<MonotonicInstant> {
        let parts = decode_task_context(context)?;
        if parts.pool != self.identity() || parts.cpu_id != self.placement().cpu_id() {
            return None;
        }
        self.inner
            .lock()
            .worker_tick_deadline(parts.worker, parts.token)
    }

    /// Accounts that a ktask-backed worker blocked in a sleepable wait.
    pub fn account_worker_blocked(
        &self,
        context: ktask::TaskExecutionContext,
        now: MonotonicInstant,
    ) -> Option<ActionBatch> {
        let parts = decode_task_context(context)?;
        let mut inner = self.inner.lock();
        if parts.pool != self.identity() || parts.cpu_id != self.placement().cpu_id() {
            return None;
        }
        inner.worker_blocked(parts.worker, parts.token, now).ok()
    }

    /// Accounts that a ktask-backed worker resumed after a sleepable wait.
    pub fn account_worker_resumed(&self, context: ktask::TaskExecutionContext) -> bool {
        let Some(parts) = decode_task_context(context) else {
            return false;
        };
        let mut inner = self.inner.lock();
        if parts.pool != self.identity() || parts.cpu_id != self.placement().cpu_id() {
            return false;
        }
        inner.worker_resumed(parts.worker, parts.token).is_ok()
    }

    /// Creates and activates this pool's configured initial worker tasks.
    ///
    /// The pool chooses worker slots from its policy's initial worker count.
    /// Callers register the runtime factory first and do not pass worker ids for
    /// initial bring-up.
    pub fn start_initial_worker_tasks(&'static self) -> usize
    where
        R: WorkerRuntime<&'static Self> + 'static,
    {
        let initial_workers = self.inner.lock().initial_workers().min(MAX_WORKERS);
        let mut started = 0usize;
        for worker_id in 0..initial_workers {
            if self.start_worker_task(WorkerId::new(worker_id)) {
                started += 1;
            }
        }
        started
    }

    /// Creates and activates one worker task for `worker`.
    fn start_worker_task(&'static self, worker: WorkerId) -> bool
    where
        R: WorkerRuntime<&'static Self> + 'static,
    {
        let Some(runtime_config) = *self.runtime.lock() else {
            return false;
        };
        if !self.inner.lock().can_install_worker(worker) {
            return false;
        }
        let placement = self.placement();
        let wake_source = Arc::new(PollEvent::new());
        let thread_wake_source = wake_source.clone();
        let task = prepare_bound_kthread(
            placement.cpu_id(),
            worker_name(placement, worker, runtime_config.pool_name),
            {
                move || {
                    // The pool may clear its WorkerThreadRef when this worker begins
                    // retirement. Hold a task ref cloned from ktask's current-task
                    // slot until the entry closure returns into ktask's exit path.
                    let _entry_task_ref = ktask::current().clone();
                    let runtime = (runtime_config.factory)(placement, worker, wake_source);
                    WorkerTask::new(self, worker, runtime).run();
                }
            },
        );

        if !self.install_worker(
            worker,
            WorkerThreadRef::new(task.clone(), thread_wake_source),
        ) {
            warn!(
                "failed to install workerpool worker {} on CPU {}",
                worker.as_usize(),
                placement.cpu_id().as_usize()
            );
            return false;
        }

        ktask::activate_task(&task);
        true
    }
}

impl<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize> ManagerTarget
    for &'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>
where
    R: WorkerRuntime<&'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>> + 'static,
{
    fn manager_should_run(&self, now: MonotonicInstant) -> bool {
        self.lock().manager_should_run(now)
    }

    fn next_management_deadline(&self, now: MonotonicInstant) -> Option<MonotonicInstant> {
        self.lock().next_management_deadline(now)
    }

    fn next_management_action(&self, now: MonotonicInstant) -> Option<ManagementAction> {
        self.lock().next_management_action(now)
    }

    fn complete_worker_spawn(
        &self,
        worker: WorkerId,
        success: bool,
        now: MonotonicInstant,
    ) -> ActionBatch {
        let result = if success {
            ManagementComplete::Spawned
        } else {
            ManagementComplete::SpawnFailed
        };
        self.lock()
            .spawn_complete(worker, result, now)
            .unwrap_or_default()
    }
}

struct KtaskManagerRuntime {
    wake_source: Arc<PollEvent>,
}

impl<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize>
    ManagerRuntime<&'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>> for KtaskManagerRuntime
where
    R: WorkerRuntime<&'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>> + 'static,
{
    fn now(&self) -> MonotonicInstant {
        ktask::monotonic_time()
    }

    fn wait_for_manager_work<const N: usize>(
        &mut self,
        pools: &[&'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>; N],
    ) {
        wait_for_manager_work(pools, &self.wake_source);
    }

    fn spawn_worker(
        &mut self,
        pool: &&'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>,
        worker: WorkerId,
    ) -> bool {
        pool.start_worker_task(worker)
    }

    fn wake_retiring_worker(
        &mut self,
        pool: &&'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>,
        worker: WorkerId,
    ) {
        let _ = pool.wake_worker(worker);
    }

    fn handle_pool_actions(
        &mut self,
        pool: &&'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>,
        actions: ActionBatch,
    ) {
        for action in actions.immediate() {
            match action {
                ImmediateAction::WakeWorker { worker } => {
                    let _ = pool.wake_worker(worker);
                }
                ImmediateAction::WakeManager => {
                    let _ = pool.wake_manager();
                }
                ImmediateAction::ArmCpuIntensiveTimer { .. } => {}
            }
        }
    }
}

/// Creates and activates one per-CPU manager task for a homogeneous pool set.
///
/// The same manager wake source is installed into every pool in `pools`, so any
/// normal pool can request slow-path lifecycle work without owning its own
/// manager task. Each pool must already have a registered worker runtime
/// factory so the manager can create dynamic workers for that pool.
pub fn start_manager_task<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize, const N: usize>(
    placement: WorkerPoolPlacement,
    name: String,
    pools: [&'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>; N],
) -> bool
where
    R: WorkerRuntime<&'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>> + 'static,
{
    let manager_wake_source = Arc::new(PollEvent::new());
    let runtime = KtaskManagerRuntime {
        wake_source: manager_wake_source.clone(),
    };
    let task = ManagerTask::new(pools, runtime);
    let manager_task = prepare_bound_kthread(placement.cpu_id(), name, move || task.run());
    let thread_ref = WorkerThreadRef::new(manager_task.clone(), manager_wake_source);

    let mut installed = true;
    for pool in pools {
        installed &= pool.install_manager_thread(thread_ref.clone());
    }
    if installed {
        ktask::activate_task(&manager_task);
    }
    installed
}

fn wait_for_manager_work<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize, const N: usize>(
    pools: &[&'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>; N],
    wake_source: &PollEvent,
) where
    R: WorkerRuntime<&'static KtaskWorkerPool<R, MAX_WORKERS, ENTRY_CAP>> + 'static,
{
    let now = ktask::monotonic_time();
    let deadline = pools
        .iter()
        .filter_map(|pool| pool.next_management_deadline(now))
        .min();
    let _ = wait_for_event_until(wake_source, deadline, || {
        let now = ktask::monotonic_time();
        pools.iter().any(|pool| pool.manager_should_run(now))
    });
}

fn wait_for_event_until(
    wake_source: &PollEvent,
    deadline: Option<MonotonicInstant>,
    mut ready: impl FnMut() -> bool,
) -> bool {
    let mut registrations = PollRegistrations::new();
    loop {
        let observed_generation = wake_source.generation();
        let event = poll_fn(|cx| {
            if ready() {
                return Poll::Ready(true);
            }

            let mut context = registrations.context(cx);
            if wake_source.register(&mut context).is_err() {
                drop(context);
                return Poll::Ready(false);
            }
            drop(context);

            if ready() || wake_source.has_changed_since(observed_generation) {
                Poll::Ready(true)
            } else {
                Poll::Pending
            }
        });
        match ktask::future::block_on(ktask::future::timeout_at(deadline, event)) {
            Ok(true) => return true,
            Ok(false) => ktask::yield_now(),
            Err(_) => return false,
        }
    }
}
