// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU bottom-half worker-pool state.

use kcpu_id_map::LogicalCpuId;
use kworkerpool::{BottomHalfWorkerPool as KworkerBottomHalfWorkerPool, WorkerPoolPlacement};

use super::pool::{BUILTIN_POOL_ENTRY_CAP, BuiltinPoolBinding, BuiltinPoolInitError};

/// Bottom-half worker pool used by built-in pools.
pub(crate) type BuiltinBottomHalfWorkerPool =
    KworkerBottomHalfWorkerPool<{ BUILTIN_POOL_ENTRY_CAP }>;

/// One bottom-half worker-pool state object.
pub(crate) struct BottomHalfWorkerPool {
    pool: BuiltinBottomHalfWorkerPool,
}

impl BottomHalfWorkerPool {
    const fn new(cpu_id: LogicalCpuId) -> Self {
        Self {
            pool: BuiltinBottomHalfWorkerPool::new(WorkerPoolPlacement::bound_to(cpu_id)),
        }
    }

    pub(crate) fn cpu_id(&self) -> LogicalCpuId {
        self.pool.placement().cpu_id()
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.pool.snapshot().installed_workers != 0
    }

    pub(crate) fn configure_empty(&self, cpu_id: LogicalCpuId) -> bool {
        self.pool
            .configure_empty(WorkerPoolPlacement::bound_to(cpu_id))
    }

    pub(crate) const fn pool(&self) -> &BuiltinBottomHalfWorkerPool {
        &self.pool
    }

    pub(crate) fn install_drain_slot(&self) -> bool {
        self.pool.install_drain_slot()
    }

    pub(crate) fn init_drain_slot(&self) {
        let _ = self.install_drain_slot();
    }
}

/// Built-in per-CPU bottom-half worker-pool set.
pub(crate) struct BottomHalfWorkerPools {
    pools: [BottomHalfWorkerPool; kbuild_config::NR_CPUS],
}

impl BottomHalfWorkerPools {
    const fn new() -> Self {
        Self {
            pools: [const { BottomHalfWorkerPool::new(LogicalCpuId::new(0)) };
                kbuild_config::NR_CPUS],
        }
    }

    pub(crate) fn cpu(
        &'static self,
        cpu_id: LogicalCpuId,
    ) -> Option<&'static BottomHalfWorkerPool> {
        self.pools.get(cpu_id.as_usize())
    }

    /// Resolves the bottom-half worker pool for one CPU.
    pub(crate) fn for_cpu(&'static self, cpu_id: LogicalCpuId) -> Option<BuiltinPoolBinding> {
        let pool = self.cpu(cpu_id)?;
        pool.is_ready()
            .then_some(BuiltinPoolBinding::BottomHalf(pool))
    }

    pub(crate) fn for_cpu_raw(
        &'static self,
        cpu_id: LogicalCpuId,
    ) -> Option<&'static BottomHalfWorkerPool> {
        self.cpu(cpu_id).filter(|pool| pool.is_ready())
    }

    /// Initializes this CPU's bottom-half worker-pool state.
    pub(crate) fn init_cpu(
        &'static self,
        cpu_id: LogicalCpuId,
    ) -> Result<(), BuiltinPoolInitError> {
        let pool = self.cpu(cpu_id).ok_or(BuiltinPoolInitError::InvalidCpu)?;
        pool.configure_empty(cpu_id)
            .then_some(())
            .ok_or(BuiltinPoolInitError::ConfigureFailed)?;
        pool.init_drain_slot();
        Ok(())
    }

    /// Returns whether the bottom-half pool exists with the expected id.
    pub(crate) fn is_cpu_ready(&'static self, cpu_id: LogicalCpuId) -> bool {
        self.cpu(cpu_id)
            .is_some_and(|pool| pool.cpu_id() == cpu_id && pool.is_ready())
    }
}

static BOTTOM_HALF_WORKER_POOLS: BottomHalfWorkerPools = BottomHalfWorkerPools::new();

/// Returns the global bottom-half worker-pool set.
pub(crate) const fn bh_worker_pools() -> &'static BottomHalfWorkerPools {
    &BOTTOM_HALF_WORKER_POOLS
}
