// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Coalesced budgeted poller.
//!
//! This module provides the NAPI-like executor shape used by subsystems which
//! need missed-wake coalescing, bounded poll rounds, and optional foreground
//! assist or lease-based advancement. A budgeted poller is a long-lived
//! execution object backed by normal work items; it is not an async task
//! executor.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kspin::SpinNoIrq;
use ktime_types::TimeSpan;

use crate::{
    QueueWorkResult, ScheduledWork, WorkQueueAllocError, WorkQueueAttrs, WorkQueueHandle,
    WorkqueueError,
};

/// Result of one budgeted poll round.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BudgetedPollProgress {
    /// Whether work is ready for immediate processing in another round.
    pub has_more: bool,
}

/// Error returned when a [`BudgetedPoller`] cannot be started.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetedPollerStartError {
    /// Poller has been destroyed and cannot be restarted.
    Destroyed,
    /// Poller start was attempted from an interrupt-like context.
    InvalidContext,
    /// The requested lane policy is not implemented by the current runtime.
    UnsupportedPolicy,
}

impl From<WorkQueueAllocError> for BudgetedPollerStartError {
    fn from(error: WorkQueueAllocError) -> Self {
        match error {
            WorkQueueAllocError::InvalidContext => Self::InvalidContext,
            WorkQueueAllocError::UnsupportedFlags => Self::UnsupportedPolicy,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum BudgetedPollerState {
    Idle           = 0b00,
    Scheduled      = 0b01,
    Running        = 0b10,
    RunningPending = 0b11,
}

impl BudgetedPollerState {
    const EXECUTOR_STATE_MASK: usize = 0b11;
    const LEASE_MASK: usize = 0b11_1111 << Self::LEASE_SHIFT;
    const LEASE_SHIFT: u32 = 2;
    const PUBLISH_GENERATION_STEP: usize = 1 << 8;

    fn from_raw(value: usize) -> Self {
        match value & Self::EXECUTOR_STATE_MASK {
            0b00 => Self::Idle,
            0b01 => Self::Scheduled,
            0b10 => Self::Running,
            0b11 => Self::RunningPending,
            _ => unreachable!("invalid budgeted poller state"),
        }
    }
}

struct BudgetedPollerInner<B, F>
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    name: &'static str,
    started: AtomicBool,
    is_queue_ready: AtomicBool,
    is_destroying: AtomicBool,
    queue: SpinNoIrq<Option<WorkQueueHandle>>,
    works: [ScheduledWork; 2],
    state: AtomicUsize,
    background_budget: B,
    lease_budget: Option<B>,
    assist_budget: B,
    lease_timeout_micros: u64,
    lease_fresh_until_micros: AtomicU64,
    max_background_rounds: usize,
    poll_once: F,
    #[cfg(unittest)]
    background_queue_attempts: AtomicUsize,
}

impl<B, F> BudgetedPollerInner<B, F>
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    fn start(&self) -> Result<(), BudgetedPollerStartError> {
        if self.is_destroying.load(Ordering::Acquire) {
            return Err(BudgetedPollerStartError::Destroyed);
        }

        if self.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        let attrs = WorkQueueAttrs::new().with_max_active(1);
        match WorkQueueHandle::alloc(self.name, attrs) {
            Ok(queue) => {
                let mut queue_slot = self.queue.lock();
                if self.is_destroying.load(Ordering::Acquire) {
                    self.started.store(false, Ordering::Release);
                    return Err(BudgetedPollerStartError::Destroyed);
                }
                *queue_slot = Some(queue);
                self.is_queue_ready.store(true, Ordering::Release);
                if self.is_destroying.load(Ordering::Acquire) {
                    self.is_queue_ready.store(false, Ordering::Release);
                    self.started.store(false, Ordering::Release);
                    return Err(BudgetedPollerStartError::Destroyed);
                }
                drop(queue_slot);
                if self.load_state() != BudgetedPollerState::Idle {
                    let _ = self.queue_background_work(0);
                }
                Ok(())
            }
            Err(error) => {
                self.started.store(false, Ordering::Release);
                Err(error.into())
            }
        }
    }

    fn notify_irq_safe(&self) -> bool {
        self.notify_irq_safe_at(None)
    }

    fn notify_irq_safe_at(&self, now_micros: Option<u64>) -> bool {
        if self.is_destroying.load(Ordering::Acquire) {
            return false;
        }

        let previous = self.publish_work();
        let is_suspended = matches!(
            BudgetedPollerState::from_raw(previous),
            BudgetedPollerState::Idle | BudgetedPollerState::Scheduled
        ) && Self::lease_count(previous) > 0
            && self.is_lease_fresh_at(previous, now_micros.unwrap_or_else(monotonic_now_micros));
        match BudgetedPollerState::from_raw(previous) {
            BudgetedPollerState::Idle | BudgetedPollerState::Scheduled if is_suspended => true,
            BudgetedPollerState::Idle => {
                let published = Self::published_raw(previous);
                if self.queue_background_work(0) {
                    true
                } else {
                    if self.is_queue_ready.load(Ordering::Acquire) {
                        self.clear_scheduled_if_unchanged(published);
                    }
                    false
                }
            }
            BudgetedPollerState::Scheduled => self.queue_background_work(0),
            BudgetedPollerState::Running | BudgetedPollerState::RunningPending => true,
        }
    }

    fn publish_work(&self) -> usize {
        // The low state bits encode pending work in both execution modes. The
        // publish generation prevents a failed queue attempt from clearing
        // `Scheduled` after a concurrent producer publishes work. The lease
        // count shares this word so notification and lease release cannot both
        // delegate queueing to the other side.
        self.state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |raw| {
                Some(Self::published_raw(raw))
            })
            .expect("budgeted poller publish update should not fail")
    }

    fn published_raw(raw: usize) -> usize {
        let state = match BudgetedPollerState::from_raw(raw) {
            BudgetedPollerState::Idle | BudgetedPollerState::Scheduled => {
                BudgetedPollerState::Scheduled
            }
            BudgetedPollerState::Running | BudgetedPollerState::RunningPending => {
                BudgetedPollerState::RunningPending
            }
        };
        (raw.wrapping_add(BudgetedPollerState::PUBLISH_GENERATION_STEP)
            & !BudgetedPollerState::EXECUTOR_STATE_MASK)
            | state as usize
    }

    fn raw_with_state(raw: usize, state: BudgetedPollerState) -> usize {
        (raw & !BudgetedPollerState::EXECUTOR_STATE_MASK) | state as usize
    }

    fn lease_count(raw: usize) -> usize {
        (raw & BudgetedPollerState::LEASE_MASK) >> BudgetedPollerState::LEASE_SHIFT
    }

    fn raw_with_lease_count(raw: usize, lease_count: usize) -> usize {
        (raw & !BudgetedPollerState::LEASE_MASK) | (lease_count << BudgetedPollerState::LEASE_SHIFT)
    }

    fn try_acquire_lease_at(&self, now_micros: u64) -> bool {
        if self.lease_budget.is_none() || self.lease_timeout_micros == 0 {
            return false;
        }

        let acquired = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |raw| {
                (Self::lease_count(raw) == 0).then(|| Self::raw_with_lease_count(raw, 1))
            })
            .is_ok();
        if acquired {
            self.refresh_lease_at(now_micros);
        }
        acquired
    }

    fn release_lease(&self) {
        let Ok(previous) = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |raw| {
                let lease_count = Self::lease_count(raw);
                (lease_count > 0).then(|| Self::raw_with_lease_count(raw, lease_count - 1))
            })
        else {
            // `destroy` clears the compound state after stopping the queue.
            return;
        };

        if Self::lease_count(previous) == 1
            && BudgetedPollerState::from_raw(previous) == BudgetedPollerState::Scheduled
        {
            let _ = self.queue_background_work(0);
        }
    }

    fn refresh_lease_at(&self, now_micros: u64) {
        self.lease_fresh_until_micros.store(
            now_micros.saturating_add(self.lease_timeout_micros),
            Ordering::Release,
        );
    }

    fn is_lease_fresh_at(&self, raw: usize, now_micros: u64) -> bool {
        if Self::lease_count(raw) == 0 {
            return false;
        }
        let fresh_until_micros = self.lease_fresh_until_micros.load(Ordering::Acquire);
        fresh_until_micros != 0 && now_micros <= fresh_until_micros
    }

    fn clear_scheduled_if_unchanged(&self, expected: usize) {
        let _ = self.state.compare_exchange(
            expected,
            Self::raw_with_state(expected, BudgetedPollerState::Idle),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn assist_once(&self) {
        self.poll_scheduled_once(self.assist_budget);
    }

    fn poll_leased_once(&self) {
        self.poll_leased_once_with_completion_time(monotonic_now_micros);
    }

    fn poll_leased_once_with_completion_time(&self, completion_time: impl FnOnce() -> u64) {
        let Some(lease_budget) = self.lease_budget else {
            return;
        };
        if !self.try_start_run() {
            return;
        }

        let progress = (self.poll_once)(lease_budget);
        self.refresh_lease_at(completion_time());
        self.finish_run(progress.has_more, 0);
    }

    fn poll_background_batch(&self, next_work: usize) {
        if !self.try_start_run() {
            return;
        }

        let mut has_more = false;
        for _ in 0..self.max_background_rounds.max(1) {
            has_more = (self.poll_once)(self.background_budget).has_more;
            if !has_more {
                break;
            }
        }

        self.finish_run(has_more, next_work);
    }

    fn poll_scheduled_once(&self, budget: B) {
        if !self.try_start_run() {
            return;
        }

        let progress = (self.poll_once)(budget);
        self.finish_run(progress.has_more, 0);
    }

    fn try_start_run(&self) -> bool {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            if BudgetedPollerState::from_raw(current) != BudgetedPollerState::Scheduled {
                return false;
            }
            match self.state.compare_exchange_weak(
                current,
                Self::raw_with_state(current, BudgetedPollerState::Running),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn finish_run(&self, has_more: bool, next_work: usize) {
        let mut current = self.state.load(Ordering::Acquire);
        let (next_state, next_raw) = loop {
            let current_state = BudgetedPollerState::from_raw(current);
            let next_state = match (current_state, has_more) {
                (BudgetedPollerState::Running, false) => BudgetedPollerState::Idle,
                (BudgetedPollerState::Running | BudgetedPollerState::RunningPending, true)
                | (BudgetedPollerState::RunningPending, false) => BudgetedPollerState::Scheduled,
                _ => unreachable!("budgeted poller released without ownership"),
            };
            match self.state.compare_exchange_weak(
                current,
                Self::raw_with_state(current, next_state),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break (next_state, Self::raw_with_state(current, next_state)),
                Err(observed) => current = observed,
            }
        };

        if next_state == BudgetedPollerState::Scheduled {
            if Self::lease_count(next_raw) > 0
                && self.is_lease_fresh_at(next_raw, monotonic_now_micros())
            {
                return;
            }
            if !self.queue_background_work(next_work) && self.is_queue_ready.load(Ordering::Acquire)
            {
                self.clear_scheduled_if_unchanged(next_raw);
            }
        }
    }

    fn queue_background_work(&self, work_index: usize) -> bool {
        #[cfg(unittest)]
        self.background_queue_attempts
            .fetch_add(1, Ordering::AcqRel);

        let queue = self.queue.lock().clone();
        let Some(queue) = queue else {
            return false;
        };

        match queue.queue_work(&self.works[work_index]) {
            QueueWorkResult::Queued => true,
            QueueWorkResult::AlreadyQueued => {
                let alternate = work_index ^ 1;
                match queue.queue_work(&self.works[alternate]) {
                    QueueWorkResult::Queued | QueueWorkResult::AlreadyQueued => true,
                    QueueWorkResult::Disabled
                    | QueueWorkResult::QueueFull
                    | QueueWorkResult::WorkerUnavailable
                    | QueueWorkResult::InvalidCpu => false,
                }
            }
            QueueWorkResult::Disabled
            | QueueWorkResult::QueueFull
            | QueueWorkResult::WorkerUnavailable
            | QueueWorkResult::InvalidCpu => false,
        }
    }

    fn destroy(&self) -> Result<(), WorkqueueError> {
        self.is_destroying.store(true, Ordering::Release);
        self.is_queue_ready.store(false, Ordering::Release);
        let queue = self.queue.lock().take();
        if let Some(queue) = queue {
            queue.destroy()?;
        }
        self.state
            .store(BudgetedPollerState::Idle as usize, Ordering::Release);
        self.started.store(false, Ordering::Release);
        Ok(())
    }

    fn load_state(&self) -> BudgetedPollerState {
        BudgetedPollerState::from_raw(self.state.load(Ordering::Acquire))
    }

    fn kick_if_stale(&self) {
        self.kick_if_stale_at(monotonic_now_micros());
    }

    fn kick_if_stale_at(&self, now_micros: u64) {
        let raw = self.state.load(Ordering::Acquire);
        if BudgetedPollerState::from_raw(raw) == BudgetedPollerState::Scheduled
            && Self::lease_count(raw) > 0
            && !self.is_lease_fresh_at(raw, now_micros)
        {
            let _ = self.queue_background_work(0);
        }
    }

    #[cfg(unittest)]
    fn force_state_for_tests(&self, state: BudgetedPollerState) {
        let _ = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |raw| {
                Some(Self::raw_with_state(raw, state))
            });
    }
}

/// A foreground lease for advancing a [`BudgetedPoller`].
///
/// While the lease is fresh, published work remains scheduled without queuing
/// another background batch. The holder must call [`Self::poll_once`] from
/// sleepable task context. Dropping the lease re-arms background polling for
/// work accumulated during the lease.
#[must_use = "dropping the lease re-arms background polling"]
pub struct PollLease<B, F>
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    inner: Arc<BudgetedPollerInner<B, F>>,
}

impl<B, F> PollLease<B, F>
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    /// Advances one scheduled round with the lease budget.
    ///
    /// The method returns without invoking the callback when another executor
    /// owns the poller or no work is scheduled. A successful round refreshes
    /// the lease deadline before deciding whether background work is needed.
    pub fn poll_once(&self) {
        self.inner.poll_leased_once();
    }
}

impl<B, F> Drop for PollLease<B, F>
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    fn drop(&mut self) {
        self.inner.release_lease();
    }
}

/// A NAPI-like coalesced budgeted poller.
///
/// Producers call [`Self::notify_irq_safe`] to publish work. At most one
/// executor owns the poller at a time. Notifications that arrive while the
/// poller is running are converted into a follow-up scheduled round instead of
/// creating a concurrent executor or losing the wake.
pub struct BudgetedPoller<B, F>
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    inner: Arc<BudgetedPollerInner<B, F>>,
}

impl<B, F> BudgetedPoller<B, F>
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    /// Creates an unstarted poller.
    ///
    /// `max_background_rounds` bounds how many immediate rounds one worker
    /// callback may run before rescheduling a follow-up callback.
    pub fn new(
        name: &'static str,
        background_budget: B,
        assist_budget: B,
        max_background_rounds: usize,
        poll_once: F,
    ) -> Self {
        Self::new_inner(
            name,
            background_budget,
            None,
            assist_budget,
            TimeSpan::ZERO,
            max_background_rounds,
            poll_once,
        )
    }

    /// Creates an unstarted poller with foreground lease support.
    ///
    /// `lease_timeout` controls how long notifications may leave scheduled
    /// work to the lease holder. A zero or sub-microsecond timeout disables
    /// lease acquisition. [`Self::new`] remains available for pollers that use
    /// only background and assist execution.
    pub fn new_with_lease(
        name: &'static str,
        background_budget: B,
        lease_budget: B,
        assist_budget: B,
        lease_timeout: TimeSpan,
        max_background_rounds: usize,
        poll_once: F,
    ) -> Self {
        Self::new_inner(
            name,
            background_budget,
            Some(lease_budget),
            assist_budget,
            lease_timeout,
            max_background_rounds,
            poll_once,
        )
    }

    fn new_inner(
        name: &'static str,
        background_budget: B,
        lease_budget: Option<B>,
        assist_budget: B,
        lease_timeout: TimeSpan,
        max_background_rounds: usize,
        poll_once: F,
    ) -> Self {
        let lease_timeout_micros = u64::try_from(lease_timeout.as_micros()).unwrap_or(u64::MAX);
        let inner = Arc::new_cyclic(|weak| BudgetedPollerInner {
            name,
            started: AtomicBool::new(false),
            is_queue_ready: AtomicBool::new(false),
            is_destroying: AtomicBool::new(false),
            queue: SpinNoIrq::new(None),
            works: [work_for(weak.clone(), 1), work_for(weak.clone(), 0)],
            state: AtomicUsize::new(BudgetedPollerState::Idle as usize),
            background_budget,
            lease_budget,
            assist_budget,
            lease_timeout_micros,
            lease_fresh_until_micros: AtomicU64::new(0),
            max_background_rounds,
            poll_once,
            #[cfg(unittest)]
            background_queue_attempts: AtomicUsize::new(0),
        });
        Self { inner }
    }

    /// Starts the backing task-context workerqueue lane.
    pub fn start(&self) -> Result<(), BudgetedPollerStartError> {
        self.inner.start()
    }

    /// Publishes work and queues the background poller if it was idle.
    ///
    /// This method does not run the poll callback and may be called from
    /// IRQ-adjacent producers.
    pub fn notify_irq_safe(&self) -> bool {
        self.inner.notify_irq_safe()
    }

    /// Assists one already scheduled round from the current task context.
    pub fn assist_once(&self) {
        self.inner.assist_once();
    }

    /// Attempts to acquire the foreground advance lease.
    ///
    /// Acquisition succeeds only after the backing queue has started and while
    /// no other lease exists. The method returns `None` from interrupt-like
    /// contexts or after destruction has begun. Callers must stop and drop all
    /// leases before calling [`Self::destroy`].
    pub fn try_lease(&self) -> Option<PollLease<B, F>> {
        if kirq::context::is_in_interrupt_context()
            || self.inner.is_destroying.load(Ordering::Acquire)
            || !self.inner.is_queue_ready.load(Ordering::Acquire)
        {
            return None;
        }

        if !self.inner.try_acquire_lease_at(monotonic_now_micros()) {
            return None;
        }
        if self.inner.is_destroying.load(Ordering::Acquire)
            || !self.inner.is_queue_ready.load(Ordering::Acquire)
        {
            self.inner.release_lease();
            return None;
        }

        Some(PollLease {
            inner: self.inner.clone(),
        })
    }

    /// Requeues scheduled work when a held lease has expired.
    ///
    /// This non-blocking check may run from a periodic task or timer callback.
    /// Queueing races are harmless because the scheduled-to-running transition
    /// still admits only one executor.
    pub fn kick_if_stale(&self) {
        self.inner.kick_if_stale();
    }

    /// Gates new backing work permanently and waits for queued/running poll callbacks.
    ///
    /// Callers must stop foreground assists and drop all leases before invoking
    /// this method.
    pub fn destroy(&self) -> Result<(), WorkqueueError> {
        self.inner.destroy()
    }

    #[cfg(unittest)]
    pub(crate) fn poll_background_batch_for_tests(&self) {
        self.inner.poll_background_batch(0);
    }

    #[cfg(unittest)]
    pub(crate) fn force_running_for_tests(&self) {
        self.inner
            .force_state_for_tests(BudgetedPollerState::Running);
    }

    #[cfg(unittest)]
    pub(crate) fn force_scheduled_for_tests(&self) {
        self.inner
            .force_state_for_tests(BudgetedPollerState::Scheduled);
    }

    #[cfg(unittest)]
    pub(crate) fn finish_run_for_tests(&self, has_more: bool) {
        self.inner.finish_run(has_more, 0);
    }

    #[cfg(unittest)]
    pub(crate) fn is_idle_for_tests(&self) -> bool {
        self.inner.load_state() == BudgetedPollerState::Idle
    }

    #[cfg(unittest)]
    pub(crate) fn is_scheduled_for_tests(&self) -> bool {
        self.inner.load_state() == BudgetedPollerState::Scheduled
    }

    #[cfg(unittest)]
    pub(crate) fn is_running_pending_for_tests(&self) -> bool {
        self.inner.load_state() == BudgetedPollerState::RunningPending
    }

    #[cfg(unittest)]
    pub fn acquire_lease_at_for_tests(&self, now_micros: u64) -> Option<PollLease<B, F>> {
        self.inner
            .try_acquire_lease_at(now_micros)
            .then(|| PollLease {
                inner: self.inner.clone(),
            })
    }

    #[cfg(unittest)]
    fn notify_irq_safe_at_for_tests(&self, now_micros: u64) -> bool {
        self.inner.notify_irq_safe_at(Some(now_micros))
    }

    #[cfg(unittest)]
    fn kick_if_stale_at_for_tests(&self, now_micros: u64) {
        self.inner.kick_if_stale_at(now_micros);
    }

    #[cfg(unittest)]
    fn lease_count_for_tests(&self) -> usize {
        BudgetedPollerInner::<B, F>::lease_count(self.inner.state.load(Ordering::Acquire))
    }

    #[cfg(unittest)]
    fn lease_fresh_until_for_tests(&self) -> u64 {
        self.inner.lease_fresh_until_micros.load(Ordering::Acquire)
    }

    #[cfg(unittest)]
    pub fn background_queue_attempts_for_tests(&self) -> usize {
        self.inner.background_queue_attempts.load(Ordering::Acquire)
    }
}

fn monotonic_now_micros() -> u64 {
    u64::try_from(ktask::monotonic_time().span_since_origin().as_micros()).unwrap_or(u64::MAX)
}

fn work_for<B, F>(inner: Weak<BudgetedPollerInner<B, F>>, next_work: usize) -> ScheduledWork
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    ScheduledWork::new(move |_work| {
        if let Some(inner) = inner.upgrade() {
            inner.poll_background_batch(next_work);
        }
    })
}

#[cfg(unittest)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::*;

    static POLL_CALLS: AtomicUsize = AtomicUsize::new(0);
    static POLL_HAS_MORE: AtomicBool = AtomicBool::new(false);

    fn poller() -> BudgetedPoller<usize, impl Fn(usize) -> BudgetedPollProgress> {
        BudgetedPoller::new("budgeted_poller_test", 8, 1, 2, |budget| {
            POLL_CALLS.fetch_add(budget, Ordering::AcqRel);
            BudgetedPollProgress { has_more: false }
        })
    }

    fn lease_poller() -> BudgetedPoller<usize, impl Fn(usize) -> BudgetedPollProgress> {
        BudgetedPoller::new_with_lease(
            "budgeted_lease_test",
            8,
            4,
            1,
            TimeSpan::from_micros(500),
            2,
            |budget| {
                POLL_CALLS.fetch_add(budget, Ordering::AcqRel);
                BudgetedPollProgress {
                    has_more: POLL_HAS_MORE.load(Ordering::Acquire),
                }
            },
        )
    }

    #[def_test(serial)]
    fn notify_before_start_keeps_work_scheduled() {
        POLL_CALLS.store(0, Ordering::Release);
        let poller = poller();

        assert_eq!(poller.notify_irq_safe(), false);
        assert!(poller.is_scheduled_for_tests());
        assert_eq!(POLL_CALLS.load(Ordering::Acquire), 0);
    }

    #[def_test(serial)]
    fn scheduled_background_batch_runs_once_and_becomes_idle() {
        POLL_CALLS.store(0, Ordering::Release);
        let poller = poller();
        poller.force_scheduled_for_tests();

        poller.poll_background_batch_for_tests();

        assert!(poller.is_idle_for_tests());
        assert_eq!(POLL_CALLS.load(Ordering::Acquire), 8);
    }

    #[def_test(serial)]
    fn notify_while_running_records_followup_round() {
        POLL_CALLS.store(0, Ordering::Release);
        let poller = poller();
        poller.force_running_for_tests();

        assert_eq!(poller.notify_irq_safe(), true);
        assert!(poller.is_running_pending_for_tests());
        poller.finish_run_for_tests(false);

        assert!(poller.is_scheduled_for_tests());
        assert_eq!(POLL_CALLS.load(Ordering::Acquire), 0);
    }

    #[def_test(serial)]
    fn lease_requires_a_started_configured_poller() {
        let poller = poller();
        assert!(poller.acquire_lease_at_for_tests(100).is_none());
        assert!(poller.try_lease().is_none());

        let poller = lease_poller();
        assert!(poller.try_lease().is_none());
    }

    #[def_test(serial)]
    fn lease_acquisition_is_exclusive_and_initially_fresh() {
        let poller = lease_poller();
        let lease = poller.acquire_lease_at_for_tests(100).unwrap();

        assert_eq!(poller.lease_count_for_tests(), 1);
        assert_eq!(poller.lease_fresh_until_for_tests(), 600);
        assert!(poller.acquire_lease_at_for_tests(200).is_none());

        drop(lease);
        assert_eq!(poller.lease_count_for_tests(), 0);
        assert_eq!(poller.background_queue_attempts_for_tests(), 0);
    }

    #[def_test(serial)]
    fn fresh_lease_suspends_notify_until_drop() {
        let poller = lease_poller();
        let lease = poller.acquire_lease_at_for_tests(100).unwrap();

        assert!(poller.notify_irq_safe_at_for_tests(200));
        assert!(poller.is_scheduled_for_tests());
        assert_eq!(poller.background_queue_attempts_for_tests(), 0);

        drop(lease);
        assert_eq!(poller.lease_count_for_tests(), 0);
        assert_eq!(poller.background_queue_attempts_for_tests(), 1);
    }

    #[def_test(serial)]
    fn leased_poll_uses_lease_budget_and_suspends_more_work() {
        POLL_CALLS.store(0, Ordering::Release);
        POLL_HAS_MORE.store(true, Ordering::Release);
        let poller = lease_poller();
        let lease = poller.acquire_lease_at_for_tests(100).unwrap();
        assert!(poller.notify_irq_safe_at_for_tests(200));

        lease
            .inner
            .poll_leased_once_with_completion_time(|| u64::MAX);

        assert_eq!(POLL_CALLS.load(Ordering::Acquire), 4);
        assert!(poller.is_scheduled_for_tests());
        assert_eq!(poller.background_queue_attempts_for_tests(), 0);

        POLL_HAS_MORE.store(false, Ordering::Release);
        drop(lease);
        assert_eq!(poller.background_queue_attempts_for_tests(), 1);
    }

    #[def_test(serial)]
    fn idle_leased_poll_does_not_refresh_deadline() {
        let poller = lease_poller();
        let lease = poller.acquire_lease_at_for_tests(100).unwrap();

        lease.inner.poll_leased_once_with_completion_time(|| 900);

        assert_eq!(poller.lease_fresh_until_for_tests(), 600);
        drop(lease);
    }

    #[def_test(serial)]
    fn expired_lease_notify_restores_background_queueing() {
        let poller = lease_poller();
        let lease = poller.acquire_lease_at_for_tests(100).unwrap();

        assert!(!poller.notify_irq_safe_at_for_tests(601));
        assert_eq!(poller.background_queue_attempts_for_tests(), 1);

        drop(lease);
    }

    #[def_test(serial)]
    fn stale_kick_queues_only_after_expiration() {
        let poller = lease_poller();
        let lease = poller.acquire_lease_at_for_tests(100).unwrap();
        poller.force_scheduled_for_tests();

        poller.kick_if_stale_at_for_tests(600);
        assert_eq!(poller.background_queue_attempts_for_tests(), 0);
        poller.kick_if_stale_at_for_tests(601);
        assert_eq!(poller.background_queue_attempts_for_tests(), 1);

        drop(lease);
    }

    #[def_test(serial)]
    fn releasing_before_notify_leaves_queueing_to_notify() {
        let poller = lease_poller();
        let lease = poller.acquire_lease_at_for_tests(100).unwrap();
        drop(lease);

        assert_eq!(poller.background_queue_attempts_for_tests(), 0);
        assert!(!poller.notify_irq_safe_at_for_tests(200));
        assert_eq!(poller.background_queue_attempts_for_tests(), 1);
    }

    #[def_test(serial)]
    fn scheduled_state_admits_only_one_executor() {
        let poller = lease_poller();
        poller.force_scheduled_for_tests();

        assert!(poller.inner.try_start_run());
        assert!(!poller.inner.try_start_run());
        poller.inner.finish_run(false, 0);
        assert!(poller.is_idle_for_tests());
    }
}
