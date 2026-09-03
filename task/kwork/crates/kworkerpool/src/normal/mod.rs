// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ordinary task-backed worker-pool implementation.

pub(crate) mod action;
mod execution;
mod ktask_pool;
pub(crate) mod manager;
mod policy;
pub(crate) mod pool;
mod state;
mod task_loop;
mod thread;
mod worker;

pub use action::{ActionBatch, ImmediateAction};
pub use execution::{WorkerPoolExecutionContext, decode_task_context};
pub use ktask_pool::{
    ExecutionTickResult, KtaskWorkerPool, WorkerPoolPlacement, WorkerRuntimeFactory,
    start_manager_task,
};
pub use policy::{
    DEFAULT_CPU_INTENSIVE_THRESHOLD, DEFAULT_CREATE_RETRY_DELAY, DEFAULT_IDLE_RETIRE_AFTER,
    WorkerPoolPolicy, WorkerPoolPolicyConfig,
};
pub use pool::{RunnableClaim, RunnableClaimer, WorkerPool};
pub use state::{RunnableCandidate, RunnableCandidateDiscard, RunnableCandidateResult};
pub use thread::{WorkerThreadRef, manager_name};
pub use worker::WorkerRuntime;
