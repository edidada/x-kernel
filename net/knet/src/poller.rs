// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Budgeted network data-plane poller.

use core::{
    future::poll_fn,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    task::Poll,
};

use klazy::lazy_static;
use kpoll::{PollContext, PollRegisterError, PollRegistrations, PollSet};

use crate::SERVICE;

const DEFAULT_BACKGROUND_BUDGET: PollBudget = PollBudget {
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
    static ref NETWORK_POLLER: NetworkPoller = NetworkPoller::new(NetworkPollerConfig {
        background_budget: DEFAULT_BACKGROUND_BUDGET,
        assist_budget: DEFAULT_ASSIST_BUDGET,
    });
}

#[derive(Clone, Copy)]
pub(crate) enum PollReason {
    Rx,
    Tx,
    Timer,
}

#[derive(Clone, Copy)]
pub(crate) struct PollBudget {
    pub rx_packets: usize,
    pub tx_packets: usize,
    pub timer_events: usize,
}

#[derive(Default)]
pub(crate) struct PollProgress {
    pub rx_packets: usize,
    pub tx_packets: usize,
    pub timer_events: usize,
    /// Whether this round increased data TX queue capacity.
    pub tx_capacity_changed: bool,
    /// Whether work is ready for immediate processing in another round.
    ///
    /// A future protocol timer deadline does not count as immediate work.
    pub has_more: bool,
}

pub(crate) struct NetworkPollerConfig {
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
    background_budget: PollBudget,
    assist_budget: PollBudget,
}

impl NetworkPoller {
    fn new(config: NetworkPollerConfig) -> Self {
        Self {
            waiters: PollSet::new(),
            tx_waiters: PollSet::new(),
            started: AtomicBool::new(false),
            state: AtomicU8::new(PollerState::Idle as u8),
            background_budget: config.background_budget,
            assist_budget: config.assist_budget,
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
    pub(crate) fn notify(&self, _reason: PollReason) {
        // The low bit means pending work in both execution modes. One RMW maps
        // Idle to Scheduled and Running to RunningPending, while same-state RMWs
        // publish producer-side work without creating redundant wakes.
        let previous = PollerState::from_raw(
            self.state
                .fetch_or(PollerState::Scheduled as u8, Ordering::AcqRel),
        );
        if previous == PollerState::Idle {
            // Every Idle-to-Scheduled transition owns its matching wake. A
            // concurrent completion either observes the pending bit or finishes
            // first and leaves this transition responsible for the wake.
            self.waiters.wake();
        }
    }

    pub(crate) fn register_tx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.tx_waiters)
    }

    /// Assists one already scheduled round from the current task context.
    pub(crate) fn assist_once(&self) {
        self.poll_scheduled_once(self.assist_budget);
    }

    fn run_task(&self) {
        let mut registrations = PollRegistrations::new();
        ktask::future::block_on(poll_fn(move |cx| {
            let mut context = registrations.context(cx);
            if context.register(&self.waiters).is_err() {
                context.wake_by_ref();
            }
            drop(context);

            // Registering before the single execution attempt closes the
            // check/register race. A pre-registration notification remains in
            // `Scheduled` for this attempt, while completion can wake the newly
            // installed registration when another round is required.
            self.poll_scheduled_once(self.background_budget);
            Poll::<()>::Pending
        }));
    }

    fn poll_scheduled_once(&self, budget: PollBudget) {
        if !self.try_start_run() {
            return;
        }

        let progress = self.run_once(budget);
        self.finish_run(progress.has_more);
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

    fn run_once(&self, budget: PollBudget) -> PollProgress {
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
        progress
    }

    fn finish_run(&self, has_more: bool) {
        let mut current = self.state.load(Ordering::Acquire);
        // This CAS loop is the sole executor-release path. Its successful CAS
        // simultaneously gives up `Running` ownership and publishes `Idle` or
        // `Scheduled`. If `notify` wins the race and installs `RunningPending`,
        // retrying converts that state to `Scheduled` instead of losing the wake.
        let next_state = loop {
            let current_state = PollerState::from_raw(current);
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
                Ok(_) => break next_state,
                Err(observed) => current = observed,
            }
        };

        if next_state == PollerState::Scheduled {
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
    use unittest::{assert, def_test};

    use super::*;

    const TEST_BUDGET: PollBudget = PollBudget {
        rx_packets: 1,
        tx_packets: 1,
        timer_events: 1,
    };

    #[def_test]
    fn only_one_executor_acquires_scheduled_work() {
        let poller = NetworkPoller::new(NetworkPollerConfig {
            background_budget: TEST_BUDGET,
            assist_budget: TEST_BUDGET,
        });
        poller
            .state
            .store(PollerState::Scheduled as u8, Ordering::Release);

        assert!(poller.try_start_run());
        assert!(!poller.try_start_run());
    }

    #[def_test]
    fn idle_notification_schedules_work() {
        let poller = NetworkPoller::new(NetworkPollerConfig {
            background_budget: TEST_BUDGET,
            assist_budget: TEST_BUDGET,
        });

        poller.notify(PollReason::Tx);
        assert!(poller.load_state() == PollerState::Scheduled);
    }

    #[def_test]
    fn notify_during_run_requests_another_round() {
        let poller = NetworkPoller::new(NetworkPollerConfig {
            background_budget: TEST_BUDGET,
            assist_budget: TEST_BUDGET,
        });
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
        let poller = NetworkPoller::new(NetworkPollerConfig {
            background_budget: TEST_BUDGET,
            assist_budget: TEST_BUDGET,
        });

        poller.assist_once();
        assert!(poller.load_state() == PollerState::Idle);
    }

    #[def_test]
    fn immediate_backlog_schedules_another_round() {
        let poller = NetworkPoller::new(NetworkPollerConfig {
            background_budget: TEST_BUDGET,
            assist_budget: TEST_BUDGET,
        });
        poller
            .state
            .store(PollerState::Running as u8, Ordering::Release);

        poller.finish_run(true);
        assert!(poller.load_state() == PollerState::Scheduled);
    }

    #[def_test]
    fn completed_run_returns_to_idle() {
        let poller = NetworkPoller::new(NetworkPollerConfig {
            background_budget: TEST_BUDGET,
            assist_budget: TEST_BUDGET,
        });
        poller
            .state
            .store(PollerState::Running as u8, Ordering::Release);

        poller.finish_run(false);
        assert!(poller.load_state() == PollerState::Idle);
    }
}
