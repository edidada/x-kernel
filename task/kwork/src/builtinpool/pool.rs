// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared built-in worker-pool bindings.

use kcpu_id_map::LogicalCpuId;
use kworkerpool::{
    ActionBatch, EntryOwner as PoolEntryOwner, ImmediateAction, KtaskWorkerPool, WorkerPoolError,
    WorkerPoolSnapshot, WorkerState,
};

use super::{
    entry::{pool_entry, pool_key, pool_owner},
    kind::{BUILTIN_POOL_ENTRY_CAP, BuiltinPoolKind},
};
use crate::runtime::BuiltinWorkerRuntime;

/// Maximum worker slots in one built-in pool.
pub(crate) const BUILTIN_WORKERS_PER_POOL: usize = kbuild_config::WORKQUEUE_WORKERS_PER_POOL;

/// Ktask-backed worker pool used by built-in pools.
pub(crate) type BuiltinKtaskWorkerPool =
    KtaskWorkerPool<BuiltinWorkerRuntime, { BUILTIN_WORKERS_PER_POOL }, { BUILTIN_POOL_ENTRY_CAP }>;

/// Built-in worker-pool snapshot normalized for diagnostics.
pub(crate) type BuiltinPoolSnapshot = WorkerPoolSnapshot<{ BUILTIN_WORKERS_PER_POOL }>;

/// Opaque binding to one built-in worker pool.
#[derive(Clone, Copy)]
pub(crate) enum BuiltinPoolBinding {
    /// Task-context system worker pool.
    System(&'static super::system::SystemWorkerPool),
    /// Bottom-half worker-pool state.
    BottomHalf(&'static super::bh::BottomHalfWorkerPool),
}

impl BuiltinPoolBinding {
    /// Returns the bound CPU.
    pub(crate) fn cpu_id(self) -> LogicalCpuId {
        match self {
            Self::System(pool) => pool.runtime().placement().cpu_id(),
            Self::BottomHalf(pool) => pool.pool().placement().cpu_id(),
        }
    }

    /// Returns a read-only state snapshot for diagnostics.
    pub(crate) fn snapshot(self) -> BuiltinPoolSnapshot {
        match self {
            Self::System(pool) => pool.runtime().lock().snapshot(),
            Self::BottomHalf(pool) => expand_bottom_half_snapshot(pool.pool().snapshot()),
        }
    }

    /// Returns a read-only state snapshot without spinning.
    pub(crate) fn try_snapshot(self) -> Option<BuiltinPoolSnapshot> {
        match self {
            Self::System(pool) => pool.runtime().try_lock().map(|pool| pool.snapshot()),
            Self::BottomHalf(pool) => pool.pool().try_snapshot().map(expand_bottom_half_snapshot),
        }
    }

    /// Applies one workqueue executor operation to this worker pool.
    pub(crate) fn apply_executor_op(
        self,
        op: kworkqueue::ExecutorOp,
    ) -> Result<(), BuiltinPoolEnqueueError> {
        let now = ktask::monotonic_time();
        let actions = match op {
            kworkqueue::ExecutorOp::EnqueueRunnable(entry) => self.enqueue_runnable(entry, now)?,
            kworkqueue::ExecutorOp::EnqueueInactive(entry) => {
                self.enqueue_deferred(entry)?;
                ActionBatch::new()
            }
            kworkqueue::ExecutorOp::Remove(entry) => {
                let _ = self.remove_entry(entry);
                ActionBatch::new()
            }
            kworkqueue::ExecutorOp::PromoteInactive { .. } => {
                return Err(BuiltinPoolEnqueueError::InvalidTransition);
            }
        };
        handle_actions(self.cpu_id(), actions);
        Ok(())
    }

    pub(crate) fn promote_one_deferred_raw(
        self,
        owner: PoolEntryOwner,
        now: ktime_types::MonotonicInstant,
    ) -> Result<Option<(kworkqueue::ExecutorEntry, ActionBatch)>, BuiltinPoolEnqueueError> {
        let promoted = match self {
            Self::System(pool) => pool.runtime().lock().promote_one_deferred(owner, now),
            Self::BottomHalf(pool) => pool
                .pool()
                .promote_one_deferred(owner, now)
                .map(|entry| (entry, ActionBatch::new())),
        };
        let Some((entry, actions)) = promoted else {
            return Ok(None);
        };
        let entry = super::entry::executor_entry(entry)
            .ok_or(BuiltinPoolEnqueueError::InvalidTransition)?;
        Ok(Some((entry, actions)))
    }

    pub(crate) fn has_executor_op_entry(self, op: kworkqueue::ExecutorOp) -> bool {
        let entry = match op {
            kworkqueue::ExecutorOp::EnqueueRunnable(entry)
            | kworkqueue::ExecutorOp::EnqueueInactive(entry)
            | kworkqueue::ExecutorOp::Remove(entry) => entry,
            kworkqueue::ExecutorOp::PromoteInactive { .. } => return false,
        };
        match self {
            Self::System(pool) => pool
                .runtime()
                .lock()
                .get_payload_mut(pool_owner(entry.owner), pool_key(entry.key))
                .is_some(),
            Self::BottomHalf(pool) => pool
                .pool()
                .has_entry(pool_owner(entry.owner), pool_key(entry.key)),
        }
    }

    fn enqueue_runnable(
        self,
        entry: kworkqueue::ExecutorEntry,
        now: ktime_types::MonotonicInstant,
    ) -> Result<ActionBatch, WorkerPoolError> {
        match self {
            Self::System(pool) => pool
                .runtime()
                .lock()
                .enqueue_runnable(pool_entry(entry), now),
            Self::BottomHalf(pool) => {
                pool.pool().enqueue_runnable(pool_entry(entry), now)?;
                Ok(ActionBatch::new())
            }
        }
    }

    fn enqueue_deferred(self, entry: kworkqueue::ExecutorEntry) -> Result<(), WorkerPoolError> {
        match self {
            Self::System(pool) => pool.runtime().lock().enqueue_deferred(pool_entry(entry)),
            Self::BottomHalf(pool) => pool.pool().enqueue_deferred(pool_entry(entry)),
        }
    }

    fn remove_entry(
        self,
        entry: kworkqueue::ExecutorEntry,
    ) -> Option<(kworkerpool::PoolEntry, kworkerpool::QueueRemoveResult)> {
        match self {
            Self::System(pool) => pool
                .runtime()
                .lock()
                .remove_entry(pool_owner(entry.owner), pool_key(entry.key)),
            Self::BottomHalf(pool) => pool
                .pool()
                .remove_entry(pool_owner(entry.owner), pool_key(entry.key)),
        }
    }
}

fn expand_bottom_half_snapshot(snapshot: WorkerPoolSnapshot<1>) -> BuiltinPoolSnapshot {
    let mut worker_states = [WorkerState::Empty; BUILTIN_WORKERS_PER_POOL];
    if BUILTIN_WORKERS_PER_POOL != 0 {
        worker_states[0] = snapshot.worker_states[0];
    }
    WorkerPoolSnapshot {
        installed_workers: snapshot.installed_workers,
        nr_creating: snapshot.nr_creating,
        nr_idle: snapshot.nr_idle,
        nr_preparing: snapshot.nr_preparing,
        nr_claiming: snapshot.nr_claiming,
        nr_running_state: snapshot.nr_running_state,
        nr_sleeping: snapshot.nr_sleeping,
        nr_retire_requested: snapshot.nr_retire_requested,
        nr_exiting: snapshot.nr_exiting,
        nr_concurrency: snapshot.nr_concurrency,
        runnable: snapshot.runnable,
        deferred: snapshot.deferred,
        worker_states,
    }
}

pub(crate) fn handle_actions(cpu_id: LogicalCpuId, actions: ActionBatch) {
    for action in actions.immediate() {
        match action {
            ImmediateAction::WakeWorker { worker } => {
                let _ =
                    super::dispatch::wake_builtin_worker(BuiltinPoolKind::Normal, cpu_id, worker);
            }
            ImmediateAction::WakeManager => {
                let _ = super::dispatch::wake_builtin_manager(BuiltinPoolKind::Normal, cpu_id);
            }
            ImmediateAction::ArmCpuIntensiveTimer { .. } => {}
        }
    }
}

/// Error returned when a workqueue executor operation cannot be applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BuiltinPoolEnqueueError {
    QueueFull,
    InvalidWorker,
    InvalidTransition,
}

impl From<WorkerPoolError> for BuiltinPoolEnqueueError {
    fn from(error: WorkerPoolError) -> Self {
        match error {
            WorkerPoolError::QueueFull(_) => Self::QueueFull,
            WorkerPoolError::InvalidWorker => Self::InvalidWorker,
            _ => Self::InvalidTransition,
        }
    }
}
