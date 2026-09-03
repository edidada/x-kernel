// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in worker pools owned by `kwork`.

mod bh;
mod dispatch;
mod entry;
mod init;
mod kind;
mod pool;
mod system;

pub(crate) use bh::BottomHalfWorkerPool as BuiltinBottomHalfPool;
pub(crate) use dispatch::{
    account_builtin_execution_blocked, account_builtin_execution_resumed,
    account_builtin_execution_tick, bottom_half_pool_for_cpu, builtin_execution_tick_deadline,
    builtin_pool_for_kind_cpu, init_system_worker_pools_for_cpu, is_builtin_pool_ready,
    system_pool_for_cpu,
};
pub(crate) use entry::{executor_entry, pool_owner};
pub use init::{BuiltinPoolInitError, SystemWorkerPoolInitResult};
pub(crate) use kind::BuiltinPoolKind;
pub(crate) use pool::{
    BuiltinKtaskWorkerPool, BuiltinPoolBinding, BuiltinPoolEnqueueError, handle_actions,
};
