// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in worker-pool initialization helpers.

use alloc::string::String;

use kcpu_id_map::LogicalCpuId;
use kworkerpool::{WorkerPoolPlacement, manager_name, start_manager_task};

use super::kind::BuiltinPoolKind;
use crate::runtime::BuiltinWorkerRuntime;

/// Result of built-in system worker-pool initialization for one CPU.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SystemWorkerPoolInitResult {
    /// Number of initial worker threads successfully started.
    pub initial_workers_started: usize,
    /// Whether the per-CPU manager thread was started.
    pub manager_started: bool,
}

/// Error returned while initializing built-in worker pools for one CPU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinPoolInitError {
    /// The requested logical CPU has no built-in pool slot.
    InvalidCpu,
    /// The selected pool was not empty and could not be configured.
    ConfigureFailed,
}

/// Initializes one task-context system worker pool.
pub(crate) fn init_system_pool(
    pool: &'static super::system::SystemWorkerPool,
    cpu_id: LogicalCpuId,
) -> SystemWorkerPoolInitResult {
    let mut result = SystemWorkerPoolInitResult::default();
    let runtime = pool.runtime();

    if !runtime.register_worker_runtime(
        BuiltinPoolKind::Normal.pool_name(),
        BuiltinWorkerRuntime::new,
    ) {
        return result;
    }
    result.initial_workers_started = runtime.start_initial_worker_tasks();

    result.manager_started = start_manager_task(
        WorkerPoolPlacement::bound_to(cpu_id),
        manager_task_name(BuiltinPoolKind::Normal, cpu_id),
        [runtime],
    );
    result
}

/// Initializes one bottom-half worker-pool state object.
pub(crate) fn init_bh_pool(pool: &'static super::bh::BottomHalfWorkerPool) {
    let _ = pool.install_drain_slot();
}

fn manager_task_name(kind: BuiltinPoolKind, cpu_id: LogicalCpuId) -> String {
    manager_name(WorkerPoolPlacement::bound_to(cpu_id), kind.pool_name())
}
