// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-process async I/O context: the ready queue, completion staging, worker
//! accounting, and the wake sources shared by submission and worker paths.
//!
//! # Locking and wake contract
//!
//! [`AsyncContext::inner`] is a sleepable [`ksync::Mutex`] guarding short,
//! non-blocking bookkeeping sections; nothing waits while holding it. Worker
//! and submitter contexts both run in sleepable task context, and no IRQ
//! handler touches this state, so no spinlock class is required here.
//!
//! Wakeups use two [`kpoll::PollSet`] sources with the detach-then-wake
//! pattern: [`AsyncContext::worker_wake`] signals queued work or shutdown,
//! [`AsyncContext::cq_wake`] signals published completions. Neither is woken
//! while a lock is held.

use alloc::{collections::VecDeque, sync::Arc};

use kerrno::KResult;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;

use crate::worker::WORKER_CAP;

/// Maximum queued submissions before the submitter must wait for the ring
/// to drain. The completion ring bounds this once the rings land; the spike
/// uses a plain queue bound to keep the test deterministic.
pub(crate) const READY_LIMIT: usize = 1024;

/// A submission prepared for execution.
///
/// The spike carries only the user cookie; the syscall-facing steps extend
/// this with resolved file handles and decoded arguments.
pub(crate) struct PreparedOp {
    /// User cookie echoed verbatim in the completion entry.
    pub(crate) cookie: u64,
}

/// A completed operation in CQE shape (cookie + signed result).
pub(crate) struct CompletedOp {
    /// Cookie echoed from the originating submission.
    pub(crate) cookie: u64,
    /// Operation result: non-negative count or negated errno.
    pub(crate) res: i64,
}

#[derive(Default)]
struct InnerState {
    /// Operations waiting for an idle worker.
    ready: VecDeque<PreparedOp>,
    /// Completions published but not yet observed by the submitter.
    completed: VecDeque<CompletedOp>,
    /// Operations dispatched to workers that have not completed yet.
    inflight: u32,
    /// Workers currently parked in the idle wait.
    idle_workers: u32,
    /// Live worker threads bound to this context.
    total_workers: u32,
    /// Monotonic ordinal source for worker names.
    spawned_workers: u32,
    /// Set once; workers exit at the next retire check.
    shutdown: bool,
}

/// Shared state of one async context (one per `asyncio_setup` fd).
pub struct AsyncContext {
    inner: Mutex<InnerState>,
    /// Woken when work becomes available or the context shuts down.
    worker_wake: PollSet,
    /// Woken when a completion is published or the context shuts down.
    cq_wake: PollSet,
}

impl AsyncContext {
    /// Creates an empty context with no workers.
    ///
    /// # Errors
    ///
    /// Reserved for ring allocation; currently infallible.
    pub fn new() -> KResult<Arc<Self>> {
        Ok(Arc::new(Self {
            inner: Mutex::new(InnerState::default()),
            worker_wake: PollSet::new(),
            cq_wake: PollSet::new(),
        }))
    }

    /// Queues an operation for worker execution and wakes an idle worker.
    ///
    /// # Errors
    ///
    /// [`kerrno::KError::BadFileDescriptor`] when the context already shut
    /// down: `shutdown` drained the queue, so an accepted operation here
    /// would never be executed, cancelled, or otherwise observable.
    /// [`kerrno::KError::WouldBlock`] when the queue is full.
    pub fn push_ready(&self, op: PreparedOp) -> KResult<()> {
        {
            let mut inner = self.inner.lock();
            if inner.shutdown {
                return Err(kerrno::KError::BadFileDescriptor);
            }
            if inner.ready.len() >= READY_LIMIT {
                return Err(kerrno::KError::WouldBlock);
            }
            inner.ready.push_back(op);
        }
        self.worker_wake.wake();
        Ok(())
    }

    /// Pops the next queued operation, if any.
    pub(crate) fn pop_ready(&self) -> Option<PreparedOp> {
        self.inner.lock().ready.pop_front()
    }

    /// Returns `true` when at least one operation is queued.
    pub(crate) fn has_ready(&self) -> bool {
        !self.inner.lock().ready.is_empty()
    }

    /// Publishes a completion and wakes completion waiters.
    pub(crate) fn publish_completion(&self, completion: CompletedOp) {
        {
            let mut inner = self.inner.lock();
            inner.inflight -= 1;
            inner.completed.push_back(completion);
        }
        self.cq_wake.wake();
    }

    /// Moves all published completions out of the context (test observation).
    #[cfg(unittest)]
    pub fn take_completions(&self) -> VecDeque<CompletedOp> {
        let mut inner = self.inner.lock();
        core::mem::take(&mut inner.completed)
    }

    /// Returns `true` when at least one completion is published.
    pub(crate) fn has_completions(&self) -> bool {
        !self.inner.lock().completed.is_empty()
    }

    /// Accounts a dispatched operation.
    pub(crate) fn account_inflight(&self) {
        self.inner.lock().inflight += 1;
    }

    /// Returns `true` after [`AsyncContext::shutdown`].
    pub fn is_shutdown(&self) -> bool {
        self.inner.lock().shutdown
    }

    /// Marks the context shut down and wakes workers and completion waiters.
    ///
    /// Operations still queued and never dispatched are cancelled with an
    /// `ASYNCIO_RES_CANCELED` completion so no user cookie is lost; operations
    /// already dispatched to a worker run to their natural completion. Never
    /// blocks: the caller is the fd `release` path or a test.
    pub fn shutdown(&self) {
        let cancelled = {
            let mut inner = self.inner.lock();
            if inner.shutdown {
                return;
            }
            inner.shutdown = true;
            core::mem::take(&mut inner.ready)
        };
        if !cancelled.is_empty() {
            let mut inner = self.inner.lock();
            inner
                .completed
                .extend(cancelled.into_iter().map(|op| CompletedOp {
                    cookie: op.cookie,
                    res: crate::abi::ASYNCIO_RES_CANCELED,
                }));
        }
        // Wake outside the lock; PollSet::wake detaches registrations under
        // its own spinlock and invokes wakers after releasing it.
        self.worker_wake.wake();
        self.cq_wake.wake();
    }

    /// Reserves a worker slot when the context is live, queued work exceeds
    /// idle workers, and the pool is below [`crate::worker::WORKER_CAP`].
    ///
    /// The eligibility check and the slot charge happen in one critical
    /// section so concurrent submitters cannot oversubscribe the cap. A
    /// `true` return must be paired with [`AsyncContext::unreserve_worker`]
    /// when the spawn fails, or naturally retires through
    /// [`AsyncContext::retire_worker`] once the worker exits.
    pub(crate) fn try_reserve_worker(&self) -> bool {
        let mut inner = self.inner.lock();
        if !inner.shutdown
            && inner.ready.len() > inner.idle_workers as usize
            && inner.total_workers < WORKER_CAP
        {
            inner.total_workers += 1;
            return true;
        }
        false
    }

    /// Returns the name ordinal for the next spawned worker.
    pub(crate) fn next_worker_ordinal(&self) -> u32 {
        let mut inner = self.inner.lock();
        inner.spawned_workers = inner.spawned_workers.wrapping_add(1);
        inner.spawned_workers
    }

    /// Releases a reserved slot without a live worker.
    pub(crate) fn unreserve_worker(&self) {
        self.inner.lock().total_workers -= 1;
    }

    /// Accounts a worker entering the idle park.
    pub(crate) fn enter_idle(&self) {
        self.inner.lock().idle_workers += 1;
    }

    /// Accounts a worker leaving the idle park.
    pub(crate) fn leave_idle(&self) {
        self.inner.lock().idle_workers -= 1;
    }

    /// Accounts a worker exiting its loop.
    pub(crate) fn retire_worker(&self) {
        self.inner.lock().total_workers -= 1;
    }

    /// Returns the live worker count (test observation).
    #[cfg(unittest)]
    pub fn total_workers(&self) -> u32 {
        self.inner.lock().total_workers
    }

    /// Returns the idle worker count (test observation).
    #[cfg(unittest)]
    pub fn idle_workers(&self) -> u32 {
        self.inner.lock().idle_workers
    }

    /// Poll-side readiness of the work source for idle workers.
    pub(crate) fn work_readiness(&self) -> IoEvents {
        let inner = self.inner.lock();
        if inner.shutdown || !inner.ready.is_empty() {
            IoEvents::IN
        } else {
            IoEvents::empty()
        }
    }

    /// Poll-side readiness of the completion source for submitters.
    // Consumed by the completion-wait path of the upcoming enter syscall and
    // by unit tests until then.
    #[allow(dead_code)]
    pub(crate) fn completion_readiness(&self) -> IoEvents {
        let inner = self.inner.lock();
        if inner.shutdown || !inner.completed.is_empty() {
            IoEvents::IN
        } else {
            IoEvents::empty()
        }
    }

    /// Registers a waker on the work source.
    pub(crate) fn register_work_wait(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.worker_wake)
    }

    /// Registers a waker on the completion source.
    #[allow(dead_code)]
    pub(crate) fn register_completion_wait(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.cq_wake)
    }
}

/// `poll_io` source for workers waiting for queued work or shutdown.
pub(crate) struct WorkSource<'a>(pub(crate) &'a AsyncContext);

impl Pollable for WorkSource<'_> {
    fn poll(&self) -> IoEvents {
        self.0.work_readiness()
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        _events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.0.register_work_wait(context)
    }
}

/// `poll_io` source for submitters waiting for completions.
// Consumed by the completion-wait path of the upcoming enter syscall and by
// unit tests until then.
#[allow(dead_code)]
pub(crate) struct CompletionSource<'a>(pub(crate) &'a AsyncContext);

impl Pollable for CompletionSource<'_> {
    fn poll(&self) -> IoEvents {
        self.0.completion_readiness()
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        _events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.0.register_completion_wait(context)
    }
}
