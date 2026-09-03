// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dispatch helpers for built-in worker pools selected by kind.

use kcpu_id_map::LogicalCpuId;
use ktime_types::MonotonicInstant;
use kworkerpool::{WorkerId, decode_task_context};

use super::{
    bh::bh_worker_pools,
    init::{BuiltinPoolInitError, SystemWorkerPoolInitResult},
    kind::BuiltinPoolKind,
    pool::BuiltinPoolBinding,
    system::system_worker_pools,
};

/// Initializes the current CPU's built-in worker pools.
pub(crate) fn init_system_worker_pools_for_cpu(
    cpu_id: LogicalCpuId,
) -> Result<SystemWorkerPoolInitResult, BuiltinPoolInitError> {
    let system = system_worker_pools().init_cpu(cpu_id)?;
    bh_worker_pools().init_cpu(cpu_id)?;
    Ok(system)
}

/// Returns whether the built-in pool for `(kind, cpu_id)` has been initialized.
pub(crate) fn is_builtin_pool_ready(kind: BuiltinPoolKind, cpu_id: LogicalCpuId) -> bool {
    match kind {
        BuiltinPoolKind::Normal => system_worker_pools().is_cpu_ready(cpu_id),
        BuiltinPoolKind::Bh => bh_worker_pools().is_cpu_ready(cpu_id),
    }
}

/// Resolves the normal system pool for one CPU.
pub(crate) fn system_pool_for_cpu(cpu_id: LogicalCpuId) -> Option<BuiltinPoolBinding> {
    system_worker_pools().for_cpu(cpu_id)
}

/// Resolves the bottom-half worker pool for one CPU.
pub(crate) fn bottom_half_pool_for_cpu(
    cpu_id: LogicalCpuId,
) -> Option<&'static super::bh::BottomHalfWorkerPool> {
    bh_worker_pools().for_cpu_raw(cpu_id)
}

/// Resolves a built-in worker pool by kind and CPU.
pub(crate) fn builtin_pool_for_kind_cpu(
    kind: BuiltinPoolKind,
    cpu_id: LogicalCpuId,
) -> Option<BuiltinPoolBinding> {
    match kind {
        BuiltinPoolKind::Normal => system_worker_pools().for_cpu(cpu_id),
        BuiltinPoolKind::Bh => bh_worker_pools().for_cpu(cpu_id),
    }
}

/// Wakes one runtime worker thread owned by a built-in pool, if available.
pub(crate) fn wake_builtin_worker(
    kind: BuiltinPoolKind,
    cpu_id: LogicalCpuId,
    worker: WorkerId,
) -> bool {
    match kind {
        BuiltinPoolKind::Normal => system_worker_pools().wake_worker(cpu_id, worker),
        BuiltinPoolKind::Bh => false,
    }
}

/// Wakes the per-CPU manager thread owned by a built-in pool.
pub(crate) fn wake_builtin_manager(kind: BuiltinPoolKind, cpu_id: LogicalCpuId) -> bool {
    match kind {
        BuiltinPoolKind::Normal => system_worker_pools().wake_manager(cpu_id),
        BuiltinPoolKind::Bh => false,
    }
}

/// Accounts one scheduler tick for a task currently running built-in pool work.
pub(crate) fn account_builtin_execution_tick(
    context: ktask::TaskExecutionContext,
    now: MonotonicInstant,
) -> Option<MonotonicInstant> {
    let decoded = decode_task_context(context)?;
    let pool = system_worker_pools().cpu(decoded.cpu_id)?;
    let result = pool.runtime().account_execution_tick(context, now)?;
    super::pool::handle_actions(pool.cpu_id(), result.actions);
    result.deadline
}

/// Reports that a built-in worker-pool execution entered a sleepable block.
pub(crate) fn account_builtin_execution_blocked(
    context: ktask::TaskExecutionContext,
    now: MonotonicInstant,
) {
    let Some(decoded) = decode_task_context(context) else {
        return;
    };
    let Some(pool) = system_worker_pools().cpu(decoded.cpu_id) else {
        return;
    };
    if let Some(actions) = pool.runtime().account_worker_blocked(context, now) {
        super::pool::handle_actions(pool.cpu_id(), actions);
    }
}

/// Reports that a built-in worker-pool execution resumed from a sleepable block.
pub(crate) fn account_builtin_execution_resumed(context: ktask::TaskExecutionContext) {
    let Some(decoded) = decode_task_context(context) else {
        return;
    };
    let Some(pool) = system_worker_pools().cpu(decoded.cpu_id) else {
        return;
    };
    let _ = pool.runtime().account_worker_resumed(context);
}

/// Returns the next CPU-intensive tick deadline for a built-in pool execution.
pub(crate) fn builtin_execution_tick_deadline(
    context: ktask::TaskExecutionContext,
) -> Option<MonotonicInstant> {
    let decoded = decode_task_context(context)?;
    let pool = system_worker_pools().cpu(decoded.cpu_id)?;
    pool.runtime().execution_tick_deadline(context)
}
