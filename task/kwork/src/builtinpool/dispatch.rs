// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dispatch helpers for built-in worker pools.

use kcpu_id_map::LogicalCpuId;
use ktime_types::MonotonicInstant;
use kworkerpool::decode_task_context;

use super::{
    bh::bh_worker_pools,
    pool::{BuiltinPoolBinding, BuiltinPoolInitError},
    system::{SystemWorkerPoolInitResult, system_worker_pools},
};

/// Initializes the current CPU's built-in worker pools.
pub(crate) fn init_system_worker_pools_for_cpu(
    cpu_id: LogicalCpuId,
) -> Result<SystemWorkerPoolInitResult, BuiltinPoolInitError> {
    let system = system_worker_pools().init_cpu(cpu_id)?;
    bh_worker_pools().init_cpu(cpu_id)?;
    Ok(system)
}

/// Returns whether the task-context system pool for `cpu_id` is initialized.
pub(crate) fn is_system_pool_ready(cpu_id: LogicalCpuId) -> bool {
    system_worker_pools().is_cpu_ready(cpu_id)
}

/// Returns whether the bottom-half pool for `cpu_id` is initialized.
#[cfg(feature = "stress_test")]
pub(crate) fn is_bottom_half_pool_ready(cpu_id: LogicalCpuId) -> bool {
    bh_worker_pools().is_cpu_ready(cpu_id)
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

/// Resolves the bottom-half worker-pool binding for one CPU.
pub(crate) fn bottom_half_pool_binding_for_cpu(cpu_id: LogicalCpuId) -> Option<BuiltinPoolBinding> {
    bh_worker_pools().for_cpu(cpu_id)
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
