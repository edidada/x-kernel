// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU task-context system worker pools.

use kcpu_id_map::LogicalCpuId;
use kworkerpool::{WorkerId, WorkerPoolPlacement, WorkerPoolPolicy};

use super::{
    init::{BuiltinPoolInitError, SystemWorkerPoolInitResult, init_system_pool},
    pool::{BUILTIN_WORKERS_PER_POOL, BuiltinKtaskWorkerPool, BuiltinPoolBinding},
};

/// One task-context system worker pool.
pub(crate) struct SystemWorkerPool {
    runtime: BuiltinKtaskWorkerPool,
}

impl SystemWorkerPool {
    const fn new(cpu_id: LogicalCpuId) -> Self {
        Self {
            runtime: BuiltinKtaskWorkerPool::new(
                WorkerPoolPlacement::bound_to(cpu_id),
                system_pool_policy(),
            ),
        }
    }

    pub(crate) fn cpu_id(&self) -> LogicalCpuId {
        self.runtime.placement().cpu_id()
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.runtime.has_manager_thread()
    }

    pub(crate) fn configure_empty(&self, cpu_id: LogicalCpuId) -> bool {
        self.runtime
            .configure_empty(WorkerPoolPlacement::bound_to(cpu_id), system_pool_policy())
    }

    pub(crate) const fn runtime(&self) -> &BuiltinKtaskWorkerPool {
        &self.runtime
    }

    pub(crate) fn wake_worker(&self, worker: WorkerId) -> bool {
        self.runtime.wake_worker(worker)
    }

    pub(crate) fn wake_manager(&self) -> bool {
        self.runtime.wake_manager()
    }
}

/// Built-in per-CPU task-context system worker-pool set.
pub(crate) struct SystemWorkerPools {
    pools: [SystemWorkerPool; kbuild_config::NR_CPUS],
}

impl SystemWorkerPools {
    const fn new() -> Self {
        Self {
            pools: [const { SystemWorkerPool::new(LogicalCpuId::new(0)) }; kbuild_config::NR_CPUS],
        }
    }

    pub(crate) fn cpu(&'static self, cpu_id: LogicalCpuId) -> Option<&'static SystemWorkerPool> {
        self.pools.get(cpu_id.as_usize())
    }

    /// Resolves the system worker pool for one CPU.
    pub(crate) fn for_cpu(&'static self, cpu_id: LogicalCpuId) -> Option<BuiltinPoolBinding> {
        let pool = self.cpu(cpu_id)?;
        pool.is_ready().then_some(BuiltinPoolBinding::System(pool))
    }

    /// Initializes this CPU's system worker pool.
    pub(crate) fn init_cpu(
        &'static self,
        cpu_id: LogicalCpuId,
    ) -> Result<SystemWorkerPoolInitResult, BuiltinPoolInitError> {
        let pool = self.cpu(cpu_id).ok_or(BuiltinPoolInitError::InvalidCpu)?;
        pool.configure_empty(cpu_id)
            .then_some(())
            .ok_or(BuiltinPoolInitError::ConfigureFailed)?;
        Ok(init_system_pool(pool, cpu_id))
    }

    /// Returns whether the system worker pool exists with the expected id.
    pub(crate) fn is_cpu_ready(&'static self, cpu_id: LogicalCpuId) -> bool {
        self.cpu(cpu_id)
            .is_some_and(|pool| pool.cpu_id() == cpu_id && pool.is_ready())
    }

    /// Wakes one system worker thread.
    pub(crate) fn wake_worker(&'static self, cpu_id: LogicalCpuId, worker: WorkerId) -> bool {
        self.cpu(cpu_id)
            .is_some_and(|pool| pool.wake_worker(worker))
    }

    /// Wakes the per-CPU system worker manager thread.
    pub(crate) fn wake_manager(&'static self, cpu_id: LogicalCpuId) -> bool {
        self.cpu(cpu_id).is_some_and(|pool| pool.wake_manager())
    }
}

static SYSTEM_WORKER_POOLS: SystemWorkerPools = SystemWorkerPools::new();

/// Returns the global task-context system worker-pool set.
pub(crate) const fn system_worker_pools() -> &'static SystemWorkerPools {
    &SYSTEM_WORKER_POOLS
}

pub(crate) const fn system_pool_policy() -> WorkerPoolPolicy {
    WorkerPoolPolicy::threaded(1, BUILTIN_WORKERS_PER_POOL)
}
