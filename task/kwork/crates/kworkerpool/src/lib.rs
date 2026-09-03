// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal worker-pool scheduling and ktask runtime for `kwork`.
//!
//! This crate owns bounded runnable/deferred queues, worker-slot state,
//! execution-token accounting, and the pool lifecycle used by `kwork`.
//! Kernel code should normally use the callback-facing `kwork` crate. Use
//! `kworkerpool` directly only when implementing another product layer that can
//! map opaque pool entries to real work, validate those entries outside the pool
//! lock, and run the accepted work in the correct execution context.
//!
//! # Pool Models
//!
//! The crate exposes two worker-pool models:
//!
//! - [`KtaskWorkerPool`] is the normal task-backed pool. It creates ktask
//!   workers, uses a manager task for dynamic worker creation and idle
//!   retirement, and supports scheduler accounting for long-running callbacks.
//! - [`BottomHalfWorkerPool`] is the bottom-half pool. It has one drain slot,
//!   no ktask worker, no manager task, and no dynamic worker lifecycle. The
//!   caller raises and runs the concrete bottom-half context.
//!
//! Both models store [`PoolEntry`] values. A pool entry is intentionally opaque:
//! [`EntrySource`], [`EntryOwner`], [`EntryKey`], and [`EntryPayload`] are
//! product-layer identifiers carried through the pool. `kworkerpool` uses the
//! owner/key for queue operations, but it does not dereference or execute work
//! by itself.
//!
//! # Choosing A Pool
//!
//! Use [`KtaskWorkerPool`] when callbacks run from sleepable task context. The
//! normal pool starts with `min_workers`, may grow up to `max_workers` when
//! runnable work is not making progress, and may retire extra idle workers.
//! [`WorkerPoolPolicy::threaded`] provides the usual policy; use
//! [`WorkerPoolPolicyConfig`] only when the product layer needs to tune
//! creation retry, idle retirement, or CPU-intensive accounting.
//!
//! Use [`BottomHalfWorkerPool`] when callbacks must run from an already-defined
//! bottom-half context, such as a softirq handler. The bottom-half pool only
//! tracks one drain slot plus runnable/deferred entries. It does not wake or
//! create workers; enqueue paths must raise the product layer's own bottom-half
//! execution context.
//!
//! # Normal Pool Use
//!
//! A normal user usually stores one or more static [`KtaskWorkerPool`] objects,
//! one per placement domain, registers a worker runtime factory, then starts
//! the initial workers and one manager task. [`WorkerRuntimeFactory`] is the
//! worker-install callback: it is called when the pool creates a ktask worker
//! for one of its own worker slots, and it returns the [`WorkerRuntime`] value
//! that will drive that worker task.
//!
//! ```rust,ignore
//! use alloc::sync::Arc;
//! use kcpu_id_map::LogicalCpuId;
//! use kpoll::PollEvent;
//! use ktime_types::MonotonicInstant;
//! use kworkerpool::{
//!     ActionBatch, KtaskWorkerPool, PoolEntry, RunnableClaim, RunnableClaimer,
//!     WorkerExecutionToken, WorkerId, WorkerPoolPlacement, WorkerPoolPolicy, WorkerRuntime,
//! };
//!
//! const MAX_WORKERS: usize = 4;
//! const ENTRY_CAP: usize = 128;
//!
//! static POOL: KtaskWorkerPool<Runtime, MAX_WORKERS, ENTRY_CAP> = KtaskWorkerPool::new(
//!     WorkerPoolPlacement::bound_to(LogicalCpuId::new(0)),
//!     WorkerPoolPolicy::threaded(1, MAX_WORKERS),
//! );
//!
//! struct Runtime {
//!     wake_source: Arc<PollEvent>,
//! }
//!
//! fn make_runtime(
//!     _placement: WorkerPoolPlacement,
//!     _worker: WorkerId,
//!     wake_source: Arc<PollEvent>,
//! ) -> Runtime {
//!     Runtime { wake_source }
//! }
//!
//! POOL.register_worker_runtime("example", make_runtime);
//! POOL.start_initial_worker_tasks();
//! kworkerpool::start_manager_task(
//!     WorkerPoolPlacement::bound_to(LogicalCpuId::new(0)),
//!     kworkerpool::manager_name(WorkerPoolPlacement::bound_to(LogicalCpuId::new(0)), "example"),
//!     [&POOL],
//! );
//! ```
//!
//! [`KtaskWorkerPool::start_initial_worker_tasks`] uses the registered factory
//! and the policy's initial worker count to choose and install worker slots.
//! Callers do not pick initial [`WorkerId`] values. Worker IDs are still
//! visible in runtime callbacks because the product layer may need them for
//! claim bookkeeping, wait-source lookup, logging, or scheduler accounting.
//!
//! Producers submit work by locking the inner [`WorkerPool`], changing only pool
//! state while the lock is held, then applying returned actions after unlock:
//!
//! ```rust,ignore
//! let actions = {
//!     let mut pool = POOL.lock();
//!     pool.enqueue_runnable(entry, ktask::monotonic_time())?
//! };
//! apply_normal_pool_actions(&POOL, actions);
//! ```
//!
//! # Actions And Runtime
//!
//! Normal pool methods never perform runtime side effects directly. They update
//! pool state and return an [`ActionBatch`] containing [`ImmediateAction`]
//! values. These actions are requests to the product layer:
//!
//! - [`ImmediateAction::WakeWorker`] means an installed worker slot should be
//!   woken.
//! - [`ImmediateAction::WakeManager`] means the manager should run lifecycle
//!   work such as dynamic worker creation or idle retirement.
//! - [`ImmediateAction::ArmCpuIntensiveTimer`] means the scheduler/accounting
//!   integration should arrange the next CPU-intensive check.
//!
//! The runtime applies those requests after the pool lock has been released.
//! This keeps wakeups, timer registration, and other subsystem callbacks out of
//! the worker-pool critical section.
//!
//! The runtime must implement two contracts. [`RunnableClaimer::claim`]
//! translates an opaque [`PoolEntry`] into subsystem-owned work after the pool
//! lock has been dropped. [`WorkerRuntime::run_claimed_work`] executes that
//! accepted work after the pool has committed the candidate as running.
//! [`WorkerRuntime::handle_worker_actions`] applies returned [`ImmediateAction`]
//! values after the pool lock is released, usually by waking workers, waking
//! the manager, or arming a CPU-intensive accounting timer.
//!
//! ```rust,ignore
//! impl RunnableClaimer for Runtime {
//!     type Accepted = ProductWork;
//!     type Stale = StaleEntry;
//!
//!     fn claim(
//!         &mut self,
//!         worker: WorkerId,
//!         token: WorkerExecutionToken,
//!         entry: PoolEntry,
//!     ) -> RunnableClaim<ProductWork, StaleEntry> {
//!         claim_product_entry(worker, token, entry)
//!     }
//!
//!     fn record_stale(&mut self, stale: StaleEntry) {
//!         record_stale_entry(stale);
//!     }
//! }
//!
//! impl WorkerRuntime<&'static KtaskWorkerPool<Runtime, MAX_WORKERS, ENTRY_CAP>> for Runtime {
//!     fn now(&self) -> MonotonicInstant {
//!         ktask::monotonic_time()
//!     }
//!
//!     fn wait_for_worker_work(
//!         &mut self,
//!         pool: &'static KtaskWorkerPool<Runtime, MAX_WORKERS, ENTRY_CAP>,
//!         worker: WorkerId,
//!     ) {
//!         wait_on_worker_wake_source(pool, worker, &self.wake_source);
//!     }
//!
//!     fn handle_worker_actions(
//!         &mut self,
//!         pool: &'static KtaskWorkerPool<Runtime, MAX_WORKERS, ENTRY_CAP>,
//!         actions: ActionBatch,
//!     ) {
//!         apply_normal_pool_actions(pool, actions);
//!     }
//!
//!     fn run_claimed_work(
//!         &mut self,
//!         pool: &'static KtaskWorkerPool<Runtime, MAX_WORKERS, ENTRY_CAP>,
//!         worker: WorkerId,
//!         token: WorkerExecutionToken,
//!         work: ProductWork,
//!     ) {
//!         run_product_work(pool, worker, token, work);
//!     }
//! }
//! ```
//!
//! Scheduler hooks should route `ktask::TaskExecutionContext` values through
//! [`KtaskWorkerPool::account_execution_tick`],
//! [`KtaskWorkerPool::execution_tick_deadline`],
//! [`KtaskWorkerPool::account_worker_blocked`], and
//! [`KtaskWorkerPool::account_worker_resumed`]. These methods use internal pool
//! identity and [`WorkerPoolPlacement`] to ignore contexts that belong to
//! another normal pool.
//!
//! # Bottom-Half Pool Use
//!
//! A bottom-half user creates a [`BottomHalfWorkerPool`], installs its drain
//! slot, enqueues [`PoolEntry`] values, and raises the product layer's own
//! bottom-half context whenever work becomes runnable.
//!
//! ```rust,ignore
//! use kcpu_id_map::LogicalCpuId;
//! use kworkerpool::{
//!     BottomHalfRunnableCandidateResult, BottomHalfWorkerPool, ParkDecision,
//!     WorkerPoolPlacement,
//! };
//!
//! const ENTRY_CAP: usize = 128;
//!
//! static BH_POOL: BottomHalfWorkerPool<ENTRY_CAP> =
//!     BottomHalfWorkerPool::new(WorkerPoolPlacement::bound_to(LogicalCpuId::new(0)));
//!
//! BH_POOL.install_drain_slot();
//! BH_POOL.enqueue_runnable(entry, ktask::monotonic_time())?;
//! raise_product_bottom_half();
//!
//! if BH_POOL.drain_ready_to_park(ktask::monotonic_time())? == ParkDecision::Run {
//!     match BH_POOL.prepare_runnable_candidate(ktask::monotonic_time())? {
//!         BottomHalfRunnableCandidateResult::Candidate(candidate) => {
//!             let work = claim_product_entry(candidate.entry)?;
//!             BH_POOL.commit_runnable_candidate(candidate.token, ktask::monotonic_time())?;
//!             run_product_bottom_half_work(work);
//!             BH_POOL.drain_finished(candidate.token, ktask::monotonic_time())?;
//!         }
//!         BottomHalfRunnableCandidateResult::Empty => {}
//!     }
//! }
//! ```
//!
//! A stale bottom-half candidate should be reported with
//! [`BottomHalfWorkerPool::discard_runnable_candidate`]. If the returned
//! [`BottomHalfRunnableCandidateDiscard::should_retry`] flag is true, the drain
//! context can immediately try to prepare another candidate.
//!
//! # Direct State-Machine Use
//!
//! [`WorkerPool`] is the unlocked normal-pool state machine used inside
//! [`KtaskWorkerPool`]. Direct users must provide their own lock, worker
//! storage, wake handling, and manager integration. Any returned [`ActionBatch`]
//! must be saved, the pool lock must be released, and only then should the
//! runtime wake workers, wake managers, or arm timers.
//!
//! The direct worker loop follows this order:
//!
//! 1. Call [`WorkerPool::worker_ready_to_park`] at the wait boundary.
//! 2. If it returns [`ParkDecision::Wait`], drop the pool lock, apply returned
//!    actions, and block until [`WorkerPool::worker_wait_ready`] becomes true.
//! 3. If it returns [`ParkDecision::Run`], call
//!    [`WorkerPool::prepare_runnable_candidate`] while holding the pool lock.
//! 4. Drop the pool lock and validate the candidate's [`PoolEntry`] with
//!    [`RunnableClaimer::claim`].
//! 5. Re-lock the pool and call [`WorkerPool::commit_runnable_candidate`] for
//!    [`RunnableClaim::Run`], or [`WorkerPool::discard_runnable_candidate`] for
//!    [`RunnableClaim::Stale`].
//! 6. After running accepted work, call [`WorkerPool::worker_finished`] with the
//!    same [`WorkerExecutionToken`].
//!
//! [`WorkerPool::enqueue_runnable`] inserts immediately runnable work and
//! evaluates dispatch. [`WorkerPool::enqueue_deferred`] stores inactive work in
//! an owner's deferred lane. [`WorkerPool::promote_one_deferred`] moves one
//! deferred entry into the runnable FIFO. [`WorkerPool::remove_entry`] removes
//! by owner/key from either lane and returns [`QueueRemoveResult`].
//!
//! [`WorkerPool::snapshot`], [`WorkerPoolSnapshot`],
//! [`WorkerPool::runnable_len`], and [`WorkerPool::installed_workers`] are
//! diagnostics. They are not lifecycle authority for product-layer work.

#![cfg_attr(not(test), no_std)]

extern crate alloc;
#[macro_use]
extern crate log;

mod bottom_half;
mod core;
mod normal;
#[cfg(unittest)]
mod tests;

pub use core::{
    EntryKey, EntryOwner, EntryPayload, EntrySource, ParkDecision, PoolEntry, QueueRemoveResult,
    WorkerExecutionToken, WorkerId, WorkerPoolError, WorkerPoolSnapshot, WorkerState,
};

pub use bottom_half::{
    BottomHalfRunnableCandidate, BottomHalfRunnableCandidateDiscard,
    BottomHalfRunnableCandidateResult, BottomHalfWorkerPool,
};
pub use normal::{
    ActionBatch, DEFAULT_CPU_INTENSIVE_THRESHOLD, DEFAULT_CREATE_RETRY_DELAY,
    DEFAULT_IDLE_RETIRE_AFTER, ExecutionTickResult, ImmediateAction, KtaskWorkerPool,
    RunnableCandidate, RunnableCandidateDiscard, RunnableCandidateResult, RunnableClaim,
    RunnableClaimer, WorkerPool, WorkerPoolExecutionContext, WorkerPoolPlacement, WorkerPoolPolicy,
    WorkerPoolPolicyConfig, WorkerRuntime, WorkerRuntimeFactory, WorkerThreadRef,
    decode_task_context, manager_name, start_manager_task,
};
