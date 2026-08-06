// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Budgeted network data-plane poller.

mod budget;
mod stats;

use core::{
    future::poll_fn,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    task::Poll,
};

use budget::{AdaptiveBudgetController, FeedDelta};
pub(crate) use budget::{
    BudgetKind, BudgetMode, PollRoundReport, ROUND_TIME_CAP, TIME_CHECK_INTERVAL,
};
use klazy::lazy_static;
use kpoll::{PollContext, PollRegisterError, PollRegistrations, PollSet};
use kspin::SpinNoPreempt;
use ktime_types::TimeSpan;
pub use stats::{PollerStatsSnapshot, poller_stats_snapshot};

use crate::SERVICE;

const DEFAULT_FIXED_BACKGROUND_BUDGET: PollBudget = PollBudget {
    rx_packets: 512,
    tx_packets: 256,
    timer_events: 32,
};

const DEFAULT_ASSIST_BUDGET: PollBudget = PollBudget {
    rx_packets: 16,
    tx_packets: 16,
    timer_events: 8,
};

lazy_static! {
    static ref NETWORK_POLLER: NetworkPoller = {
        let mode = if cfg!(feature = "fixed-poll-budget") {
            BudgetMode::Fixed
        } else {
            BudgetMode::Adaptive
        };
        NetworkPoller::new(NetworkPollerConfig {
            mode,
            background_budget: DEFAULT_FIXED_BACKGROUND_BUDGET,
            assist_budget: DEFAULT_ASSIST_BUDGET,
        })
    };
}

#[derive(Clone, Copy)]
pub(crate) enum PollReason {
    Rx,
    RxWindow,
    Tx,
    Timer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PollBudget {
    pub rx_packets: usize,
    pub tx_packets: usize,
    pub timer_events: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PollProgress {
    pub rx_packets: usize,
    pub tx_packets: usize,
    pub timer_events: usize,
    /// Whether this round increased data TX queue capacity.
    pub tx_capacity_changed: bool,
    /// RX packet budget was fully consumed and immediate RX/ingress remains.
    pub rx_exhausted: bool,
    /// TX packet budget was fully consumed and queued TX remains.
    pub tx_exhausted: bool,
    /// Timer budget slot was open and at least one due timer remains.
    pub timer_exhausted: bool,
    /// Immediate RX device or ingress work remains.
    #[expect(dead_code)]
    pub rx_more: bool,
    /// Queued TX work remains.
    #[expect(dead_code)]
    pub tx_more: bool,
    /// Whether any already-due timer remains after this round.
    #[expect(dead_code)]
    pub timer_more: bool,
    /// Whether work is ready for immediate processing in another round.
    ///
    /// A future protocol timer deadline does not count as immediate work.
    pub has_more: bool,
    /// Wall time spent in this `poll_budgeted` round.
    pub elapsed: TimeSpan,
    /// Which budget class first hit [`ROUND_TIME_CAP`], if any.
    pub overrun: Option<BudgetKind>,
}

pub(crate) struct NetworkPollerConfig {
    mode: BudgetMode,
    background_budget: PollBudget,
    assist_budget: PollBudget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum PollerState {
    Idle           = 0b00,
    Scheduled      = 0b01,
    Running        = 0b10,
    RunningPending = 0b11,
}

impl PollerState {
    fn from_raw(value: u8) -> Self {
        match value {
            0b00 => Self::Idle,
            0b01 => Self::Scheduled,
            0b10 => Self::Running,
            0b11 => Self::RunningPending,
            _ => unreachable!("invalid network poller state"),
        }
    }
}

pub(crate) struct NetworkPoller {
    waiters: PollSet,
    tx_waiters: PollSet,
    started: AtomicBool,
    state: AtomicU8,
    mode: BudgetMode,
    background_budget: PollBudget,
    assist_budget: PollBudget,
    /// Adaptive budgets. Never held together with `SERVICE` locks.
    controller: SpinNoPreempt<AdaptiveBudgetController>,
}

impl NetworkPoller {
    fn new(config: NetworkPollerConfig) -> Self {
        Self {
            waiters: PollSet::new(),
            tx_waiters: PollSet::new(),
            started: AtomicBool::new(false),
            state: AtomicU8::new(PollerState::Idle as u8),
            mode: config.mode,
            background_budget: config.background_budget,
            assist_budget: config.assist_budget,
            controller: SpinNoPreempt::new(AdaptiveBudgetController::new()),
        }
    }

    /// Starts the persistent background task during network initialization.
    pub(crate) fn start(&'static self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        ktask::spawn_with_name(poller_task, "knet-poller".into());
    }

    /// Publishes network work and wakes the background task when it is idle.
    ///
    /// This method may run in IRQ-adjacent paths and does not acquire a sleepable
    /// lock or execute the network data plane.
    pub(crate) fn notify(&self, reason: PollReason) {
        let previous = self.publish_work(reason);
        if previous == PollerState::Idle {
            // Every Idle-to-Scheduled transition owns its matching wake. A
            // concurrent completion either observes the pending bit or finishes
            // first and leaves this transition responsible for the wake.
            self.waiters.wake();
        }
    }

    /// Publishes RX work and executes one budgeted round in the RX waiter task.
    ///
    /// The caller must run in task context without holding spinlocks or other
    /// device and data-plane locks because this path may acquire sleepable
    /// mutexes. Poller ownership still applies; remaining work is rescheduled.
    pub(crate) fn publish_and_poll_rx(&self) {
        self.publish_and_poll_rx_with(|budget| self.run_once(budget, true));
    }

    fn publish_work(&self, _reason: PollReason) -> PollerState {
        // The low bit means pending work in both execution modes. One RMW maps
        // Idle to Scheduled and Running to RunningPending, while same-state RMWs
        // publish producer-side work without creating redundant wakes.
        PollerState::from_raw(
            self.state
                .fetch_or(PollerState::Scheduled as u8, Ordering::AcqRel),
        )
    }

    fn publish_and_poll_rx_with(&self, poll_once: impl FnOnce(PollBudget) -> PollProgress) {
        // This task immediately competes for ownership, so an Idle transition
        // does not need a second task wake. A concurrent owner observes
        // RunningPending and schedules the follow-up round when it finishes.
        self.publish_work(PollReason::Rx);
        self.poll_scheduled_with(poll_once);
    }

    pub(crate) fn register_tx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.tx_waiters)
    }

    /// Assists one already scheduled round from the current task context.
    ///
    /// Assist uses a fixed small budget and does not feed the adaptive
    /// controller, so opportunistic assist rounds cannot bias growth.
    pub(crate) fn assist_once(&self) {
        if !self.try_start_run() {
            return;
        }
        let progress = self.run_once(self.assist_budget, false);
        self.finish_run(progress.has_more);
    }

    fn run_task(&self) {
        let mut registrations = PollRegistrations::new();
        ktask::future::block_on(poll_fn(move |cx| {
            let mut context = registrations.context(cx);
            if context.register(&self.waiters).is_err() {
                context.wake_by_ref();
            }
            drop(context);

            // Registering before the first execution attempt closes the
            // check/register race. A pre-registration notification remains in
            // `Scheduled` for this round, while completion can wake the newly
            // installed registration when another round is required.
            self.poll_scheduled_round();
            Poll::<()>::Pending
        }));
    }

    fn poll_scheduled_round(&self) {
        self.poll_scheduled_with(|budget| self.run_once(budget, true));
    }

    fn poll_scheduled_with(&self, poll_once: impl FnOnce(PollBudget) -> PollProgress) {
        if !self.try_start_run() {
            return;
        }

        let budget = self.budget_for_owned_round();
        let progress = poll_once(budget);
        self.finish_run(progress.has_more);
    }

    fn budget_for_owned_round(&self) -> PollBudget {
        match self.mode {
            BudgetMode::Fixed => self.background_budget,
            BudgetMode::Adaptive => {
                // Drop the controller lock before touching SERVICE.
                self.controller.lock().snapshot()
            }
        }
    }

    fn try_start_run(&self) -> bool {
        self.state
            .compare_exchange(
                PollerState::Scheduled as u8,
                PollerState::Running as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn run_once(&self, budget: PollBudget, feed_controller: bool) -> PollProgress {
        if !SERVICE.is_inited() {
            return PollProgress::default();
        }

        let progress = SERVICE.poll_budgeted(budget);
        if progress.tx_capacity_changed {
            self.tx_waiters.wake();
        }
        debug_assert!(progress.rx_packets <= budget.rx_packets);
        debug_assert!(progress.tx_packets <= budget.tx_packets);
        debug_assert!(progress.timer_events <= budget.timer_events);
        debug_assert!(
            progress
                .overrun
                .is_none_or(|_| progress.elapsed >= ROUND_TIME_CAP)
        );

        let feed = if feed_controller && self.mode == BudgetMode::Adaptive {
            let report = PollRoundReport {
                rx_exhausted: progress.rx_exhausted,
                tx_exhausted: progress.tx_exhausted,
                timer_exhausted: progress.timer_exhausted,
                elapsed: progress.elapsed,
                overrun: progress.overrun,
                is_idle: progress.rx_packets == 0
                    && progress.tx_packets == 0
                    && progress.timer_events == 0
                    && !progress.has_more
                    && progress.overrun.is_none(),
            };
            // Controller lock is released before returning to the caller; it
            // is never held together with SERVICE locks.
            self.controller.lock().feed(&report)
        } else {
            FeedDelta::default()
        };

        stats::record_round(
            progress.rx_packets,
            progress.tx_packets,
            progress.timer_events,
            progress.elapsed,
            progress.overrun,
            feed,
        );
        progress
    }

    fn finish_run(&self, has_more: bool) {
        let mut current = self.state.load(Ordering::Acquire);
        // This CAS loop is the sole executor-release path. Its successful CAS
        // simultaneously gives up `Running` ownership and publishes `Idle` or
        // `Scheduled`. If `notify` wins the race and installs `RunningPending`,
        // retrying converts that state to `Scheduled` instead of losing the wake.
        let (next_state, observed_pending) = loop {
            let current_state = PollerState::from_raw(current);
            let observed_pending = matches!(current_state, PollerState::RunningPending);
            let next_state = match (current_state, has_more) {
                (PollerState::Running, false) => PollerState::Idle,
                (PollerState::Running | PollerState::RunningPending, true)
                | (PollerState::RunningPending, false) => PollerState::Scheduled,
                _ => unreachable!("network poller released without ownership"),
            };
            match self.state.compare_exchange_weak(
                current,
                next_state as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break (next_state, observed_pending),
                Err(observed) => current = observed,
            }
        };

        if next_state == PollerState::Scheduled {
            if has_more {
                stats::record_backlog_reschedule();
            }
            if observed_pending {
                stats::record_notify_reschedule();
            }
            self.waiters.wake();
        }
    }

    #[cfg(unittest)]
    fn load_state(&self) -> PollerState {
        PollerState::from_raw(self.state.load(Ordering::Acquire))
    }
}

pub(crate) fn network_poller() -> &'static NetworkPoller {
    &NETWORK_POLLER
}

fn poller_task() {
    NETWORK_POLLER.run_task();
}

#[cfg(unittest)]
mod tests {
    use core::cell::Cell;

    use unittest::{assert, def_test};

    use super::*;

    const TEST_BUDGET: PollBudget = PollBudget {
        rx_packets: 1,
        tx_packets: 1,
        timer_events: 1,
    };

    fn test_poller() -> NetworkPoller {
        NetworkPoller::new(NetworkPollerConfig {
            mode: BudgetMode::Fixed,
            background_budget: TEST_BUDGET,
            assist_budget: TEST_BUDGET,
        })
    }

    #[def_test]
    fn only_one_executor_acquires_scheduled_work() {
        let poller = test_poller();
        poller
            .state
            .store(PollerState::Scheduled as u8, Ordering::Release);

        assert!(poller.try_start_run());
        assert!(!poller.try_start_run());
    }

    #[def_test]
    fn idle_notification_schedules_work() {
        let poller = test_poller();

        poller.notify(PollReason::Tx);
        assert!(poller.load_state() == PollerState::Scheduled);
    }

    #[def_test]
    fn rx_task_publishes_and_polls_inline() {
        let poller = test_poller();
        let rounds = Cell::new(0);
        let ownership_held = Cell::new(false);

        poller.publish_and_poll_rx_with(|_| {
            rounds.set(rounds.get() + 1);
            ownership_held.set(poller.load_state() == PollerState::Running);
            PollProgress::default()
        });

        assert!(rounds.get() == 1);
        assert!(ownership_held.get());
        assert!(poller.load_state() == PollerState::Idle);
    }

    #[def_test]
    fn rx_task_defers_to_a_running_owner() {
        let poller = test_poller();
        poller
            .state
            .store(PollerState::Running as u8, Ordering::Release);
        let rounds = Cell::new(0);

        poller.publish_and_poll_rx_with(|_| {
            rounds.set(rounds.get() + 1);
            PollProgress::default()
        });

        assert!(rounds.get() == 0);
        assert!(poller.load_state() == PollerState::RunningPending);
        poller.finish_run(false);
        assert!(poller.load_state() == PollerState::Scheduled);
    }

    #[def_test]
    fn notify_during_run_requests_another_round() {
        let poller = test_poller();
        poller
            .state
            .store(PollerState::Running as u8, Ordering::Release);

        poller.notify(PollReason::Rx);
        assert!(poller.load_state() == PollerState::RunningPending);
        poller.finish_run(false);
        assert!(poller.load_state() == PollerState::Scheduled);
    }

    #[def_test]
    fn assist_cannot_acquire_idle_poller() {
        let poller = test_poller();

        poller.assist_once();
        assert!(poller.load_state() == PollerState::Idle);
    }

    #[def_test]
    fn immediate_backlog_schedules_another_round() {
        let poller = test_poller();
        poller
            .state
            .store(PollerState::Running as u8, Ordering::Release);

        poller.finish_run(true);
        assert!(poller.load_state() == PollerState::Scheduled);
    }

    #[def_test]
    fn completed_run_returns_to_idle() {
        let poller = test_poller();
        poller
            .state
            .store(PollerState::Running as u8, Ordering::Release);

        poller.finish_run(false);
        assert!(poller.load_state() == PollerState::Idle);
    }

    #[def_test]
    fn scheduled_round_runs_exactly_once() {
        let poller = test_poller();
        poller
            .state
            .store(PollerState::Scheduled as u8, Ordering::Release);
        let rounds = Cell::new(0);
        let ownership_held = Cell::new(true);

        poller.poll_scheduled_with(|_| {
            ownership_held.set(
                ownership_held.get()
                    && poller.load_state() == PollerState::Running
                    && !poller.try_start_run(),
            );
            rounds.set(rounds.get() + 1);
            PollProgress {
                has_more: true,
                ..PollProgress::default()
            }
        });

        assert!(rounds.get() == 1);
        assert!(ownership_held.get());
        assert!(poller.load_state() == PollerState::Scheduled);
    }

    #[def_test]
    fn notification_during_scheduled_round_reschedules() {
        let poller = test_poller();
        poller
            .state
            .store(PollerState::Scheduled as u8, Ordering::Release);
        let pending_observed = Cell::new(false);

        poller.poll_scheduled_with(|_| {
            poller.notify(PollReason::Rx);
            pending_observed.set(poller.load_state() == PollerState::RunningPending);
            PollProgress::default()
        });

        assert!(pending_observed.get());
        assert!(poller.load_state() == PollerState::Scheduled);
    }
}
