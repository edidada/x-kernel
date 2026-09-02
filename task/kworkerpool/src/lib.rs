// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel worker-pool scheduling and ktask runtime.
//!
//! This crate owns worker-pool state, runnable/deferred pool queues, and
//! lifecycle management decisions. It also owns ktask-backed worker and
//! manager task loops. Built-in system pool instances live in `kwork`.
//!
//! Most kernel code should use a product layer such as `kwork`. Use this crate
//! directly only when a subsystem needs its own bounded worker-pool scheduler
//! and can provide the runtime glue that validates opaque entries, wakes worker
//! contexts, and runs callbacks.
//!
//! # Core Types
//!
//! - [`WorkerPool`] is the core state machine.
//! - [`PoolEntry`] is the opaque entry stored by a pool. The pool never
//!   interprets [`EntrySource`], [`EntryOwner`], [`EntryKey`], or
//!   [`EntryPayload`]; the product layer maps those values to real work.
//! - [`WorkerPoolPolicy`] configures worker count, scalability, retirement,
//!   dispatch target, and CPU-intensive accounting. Use
//!   [`WorkerPoolPolicy::fixed_threaded`] or
//!   [`WorkerPoolPolicy::scalable_threaded`] for ordinary pools; use
//!   [`WorkerPoolPolicyConfig`] only when the runtime needs every lifecycle
//!   policy knob.
//! - [`WorkerPoolPlacement`] names where ktask-backed worker and manager tasks
//!   should run. It is separate from [`PoolId`] so runtime placement does not
//!   define logical pool identity.
//! - [`ActionBatch`] carries [`ImmediateAction`] values that must be executed
//!   by the runtime after the pool lock has been dropped.
//! - [`RunnableClaimer`] validates a popped [`PoolEntry`] outside the pool lock.
//! - [`WorkerRuntime`] supplies time, wait, action handling, and callback
//!   execution for the pool-owned worker loop.
//! - [`KtaskWorkerPool`] adds ktask-backed worker and manager threads around
//!   [`WorkerPool`].
//! - [`WorkerPoolExecutionContext`] and [`decode_task_context`] let scheduler
//!   accounting code recognize a ktask that is currently running pool work.
//!
//! # Direct State-Machine Use
//!
//! Direct users own locking and runtime integration. Protect [`WorkerPool`] with
//! the runtime's pool-instance lock. Any returned [`ActionBatch`] must be saved,
//! the pool lock must be released, and only then should the runtime wake
//! workers, raise bottom halves, or arm timers.
//!
//! ```rust
//! use kcpu_id_map::LogicalCpuId;
//! use kworkerpool::{
//!     EntryKey, EntryOwner, EntryPayload, EntrySource, PoolEntry, PoolId, PoolKind, WorkerPool,
//!     WorkerPoolPolicy,
//! };
//!
//! let policy = WorkerPoolPolicy::scalable_threaded(1, 4);
//!
//! let pool_id = PoolId::new(PoolKind::new(0), LogicalCpuId::new(0));
//! let mut pool: WorkerPool<(), 4, 128> = WorkerPool::new(pool_id, policy);
//! let entry = PoolEntry::new(
//!     EntrySource::new(1),
//!     EntryOwner::new(1),
//!     EntryKey::new(1),
//!     EntryPayload::new(1),
//! );
//! let _actions = pool
//!     .enqueue_runnable(entry, ktask::monotonic_time())
//!     .unwrap();
//! ```
//!
//! A worker-side runtime drives the public state machine in this order:
//!
//! 1. Call [`WorkerPool::worker_ready_to_park`] at the wait boundary.
//! 2. If it returns [`ParkDecision::Wait`], drop the pool lock, execute returned
//!    actions, and block until [`WorkerPool::worker_wait_ready`] becomes true.
//! 3. If it returns [`ParkDecision::Run`], call
//!    [`WorkerPool::prepare_runnable_candidate`] while holding the pool lock.
//! 4. Drop the pool lock and pass the candidate's [`PoolEntry`] to
//!    [`RunnableClaimer::claim`].
//! 5. Re-lock the pool and call [`WorkerPool::commit_runnable_candidate`] for
//!    [`RunnableClaim::Run`], or [`WorkerPool::discard_runnable_candidate`] for
//!    [`RunnableClaim::Stale`].
//! 6. After running accepted work, call [`WorkerPool::worker_finished`] with the
//!    same [`WorkerExecutionToken`].
//!
//! The token returned in [`RunnableCandidate`] identifies one worker-slot
//! execution. Finish, discard, tick, block, and resume paths use that token to
//! reject stale operations after a worker slot is reused.
//!
//! # Queue Operations
//!
//! [`WorkerPool::enqueue_runnable`] inserts immediately runnable work and
//! evaluates dispatch. [`WorkerPool::enqueue_deferred`] stores inactive work in
//! the entry owner's deferred lane. [`WorkerPool::promote_one_deferred`] moves
//! one deferred entry for an owner into the runnable FIFO and evaluates
//! dispatch. [`WorkerPool::remove_entry`] removes by owner/key from either lane,
//! returning [`QueueRemoveResult`].
//!
//! [`WorkerPool::snapshot`], [`WorkerPoolSnapshot`],
//! [`WorkerPool::runnable_len`], and [`WorkerPool::installed_workers`] are
//! diagnostic helpers; they should not be used to enforce external lifecycle
//! invariants.
//!
//! # Policy Selection
//!
//! [`WorkerPoolPolicy::fixed_threaded`] is for task-backed runtimes that install
//! a known number of workers and do not need dynamic growth or idle retirement.
//! [`WorkerPoolPolicy::scalable_threaded`] is for task-backed pools that should
//! grow when runnable work stalls and shrink after extra workers stay idle.
//! [`WorkerPoolPolicy::bottom_half`] is for runtimes that execute work from a
//! bottom-half style context instead of waking task-backed workers.
//!
//! [`WorkerPoolPolicyConfig`] is the advanced constructor input. It exposes
//! internal lifecycle switches:
//!
//! - `dispatch_target` selects whether dispatch wakes task-backed workers or
//!   raises a bottom-half style action.
//! - `scalable` lets the pool request extra runtime workers when runnable work
//!   is not making progress.
//! - `idle_retire` lets the pool retire idle workers above `min_workers`.
//! - `create_retry_delay` is the backoff before trying worker creation again
//!   after a creation failure.
//!
//! Prefer the named constructors unless the runtime has a specific reason to
//! tune these fields.
//!
//! # Ktask-Backed Use
//!
//! [`KtaskWorkerPool`] owns a locked [`WorkerPool`], a [`WorkerPoolPlacement`],
//! and ktask wake handles. A subsystem that uses it usually stores one static
//! pool per CPU, configures each pool with [`WorkerPoolPlacement`], provides a
//! [`WorkerRuntimeFactory`], starts initial workers with
//! [`KtaskWorkerPool::start_worker_task`], and starts one manager for a
//! scalable pool set with [`start_manager_task`].
//!
//! The runtime type created by [`WorkerRuntimeFactory`] implements
//! [`WorkerRuntime`] and [`RunnableClaimer`]. Its `claim` method translates an
//! opaque [`PoolEntry`] into subsystem-owned work after the pool lock has been
//! dropped. Its `run_claimed_work` method executes the accepted work after the
//! candidate has been committed as running.
//!
//! Scheduler accounting should route the current task's
//! `ktask::TaskExecutionContext` through
//! [`KtaskWorkerPool::account_execution_tick`],
//! [`KtaskWorkerPool::execution_tick_deadline`],
//! [`KtaskWorkerPool::account_worker_blocked`], and
//! [`KtaskWorkerPool::account_worker_resumed`]. These helpers ignore contexts
//! that belong to another pool.
//!
//! # Manager And Wakeups
//!
//! Scalable pools use a manager task to create dynamic workers and retire idle
//! ones. The public ktask helper [`start_manager_task`] wires one manager wake
//! source into a homogeneous set of [`KtaskWorkerPool`] instances. Fast paths do
//! not create workers directly; they return [`ImmediateAction::WakeManager`]
//! when manager work is needed.
//!
//! Fixed-size pools do not request new workers or idle retirement. They rely on
//! already installed workers and may return [`ImmediateAction::RaiseBottomHalf`]
//! instead of worker wakeups, depending on policy.

#![cfg_attr(not(test), no_std)]

extern crate alloc;
#[macro_use]
extern crate log;

mod action;
mod entry;
mod execution;
mod id;
mod ktask_pool;
mod manager;
mod policy;
mod pool;
mod queue;
mod task_loop;
#[cfg(unittest)]
mod tests;
mod thread;
mod worker;

pub use action::{ActionBatch, ImmediateAction};
pub use entry::{EntryKey, EntryOwner, EntryPayload, EntrySource};
pub use execution::{WorkerPoolExecutionContext, decode_task_context};
pub use id::{PoolId, PoolKind, WorkerId};
pub use ktask_pool::{
    ExecutionTickResult, KtaskWorkerPool, PoolNameResolver, WorkerPoolPlacement,
    WorkerRuntimeFactory, start_manager_task,
};
pub use policy::{
    DEFAULT_CPU_INTENSIVE_THRESHOLD, DEFAULT_CREATE_RETRY_DELAY, DEFAULT_IDLE_RETIRE_AFTER,
    WorkerPoolDispatchTarget, WorkerPoolPolicy, WorkerPoolPolicyConfig,
};
pub use pool::{
    ParkDecision, RunnableCandidate, RunnableCandidateDiscard, RunnableCandidateResult,
    RunnableClaim, RunnableClaimer, WorkerPool, WorkerPoolError, WorkerPoolSnapshot,
};
pub use queue::{PoolEntry, QueueRemoveResult};
pub use thread::{WorkerThreadRef, manager_name};
pub use worker::{WorkerExecutionToken, WorkerRuntime, WorkerState};
