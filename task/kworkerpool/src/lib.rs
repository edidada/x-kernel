// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel worker-pool scheduling core.
//!
//! This crate owns worker-pool state, runnable/deferred pool queues, and
//! lifecycle management decisions. It intentionally does not know about
//! workqueues, pool bindings, flush colors, barriers, or provider task APIs.

#![cfg_attr(not(test), no_std)]

mod action;
mod id;
mod manager;
mod policy;
mod pool;
mod queue;
#[cfg(unittest)]
mod tests;
mod worker;

pub use action::{ActionBatch, ImmediateAction, ManagementAction};
pub use id::{BuiltinPoolKind, PoolId, WorkerId};
pub use manager::MANAGER_ACTION_BUDGET_PER_POOL_PASS;
pub use policy::{WorkerPoolPolicy, WorkerPoolPolicyConfig};
pub use pool::{
    ClaimFailedResult, DispatchResult, ManagementComplete, ParkDecision, WorkerPool,
    WorkerPoolError,
};
pub use queue::{PoolEntry, PoolRunQueue, QueueRemoveResult};
pub use worker::{
    ExecutionAccounting, WorkerExecutionToken, WorkerRecord, WorkerSleepTransition, WorkerState,
    WorkerWakePlan,
};
