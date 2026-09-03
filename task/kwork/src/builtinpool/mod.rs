// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in worker pools owned by `kwork`.

mod bh;
mod dispatch;
mod entry;
mod pool;
mod system;

pub(crate) use bh::BottomHalfWorkerPool as BuiltinBottomHalfPool;
#[cfg(feature = "stress_test")]
pub(crate) use dispatch::is_bottom_half_pool_ready;
pub(crate) use dispatch::{
    account_builtin_execution_blocked, account_builtin_execution_resumed,
    account_builtin_execution_tick, bottom_half_pool_binding_for_cpu, bottom_half_pool_for_cpu,
    builtin_execution_tick_deadline, init_system_worker_pools_for_cpu, is_system_pool_ready,
    system_pool_for_cpu,
};
pub(crate) use entry::{executor_entry, pool_owner};
pub use pool::BuiltinPoolInitError;
pub(crate) use pool::{
    BuiltinKtaskWorkerPool, BuiltinPoolBinding, BuiltinPoolEnqueueError, handle_actions,
};
pub use system::SystemWorkerPoolInitResult;
