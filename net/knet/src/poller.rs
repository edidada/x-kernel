// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Budgeted network data-plane poller.

use core::{
    future::poll_fn,
    sync::atomic::{AtomicBool, Ordering},
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

const DEFAULT_BACKGROUND_MAX_ROUNDS: usize = 4;

lazy_static! {
    static ref NETWORK_POLLER: NetworkPoller = NetworkPoller::new(NetworkPollerConfig {
        background_budget: DEFAULT_BACKGROUND_BUDGET,
        assist_budget: DEFAULT_ASSIST_BUDGET,
        background_max_rounds: DEFAULT_BACKGROUND_MAX_ROUNDS,
    });
}

#[derive(Clone, Copy)]
pub(crate) enum PollReason {
    RxIrq,
    Assist,
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
    pub has_more: bool,
}

pub(crate) struct NetworkPollerConfig {
    background_budget: PollBudget,
    assist_budget: PollBudget,
    background_max_rounds: usize,
}

pub(crate) struct NetworkPoller {
    waiters: PollSet,
    tx_waiters: PollSet,
    started: AtomicBool,
    scheduled: AtomicBool,
    missed: AtomicBool,
    active: AtomicBool,
    background_budget: PollBudget,
    assist_budget: PollBudget,
    background_max_rounds: usize,
}

impl NetworkPoller {
    fn new(config: NetworkPollerConfig) -> Self {
        Self {
            waiters: PollSet::new(),
            tx_waiters: PollSet::new(),
            started: AtomicBool::new(false),
            scheduled: AtomicBool::new(false),
            missed: AtomicBool::new(false),
            active: AtomicBool::new(false),
            background_budget: config.background_budget,
            assist_budget: config.assist_budget,
            background_max_rounds: config.background_max_rounds,
        }
    }

    pub(crate) fn start(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }

        ktask::spawn_with_name(poller_task, "knet-poller".into());
    }

    pub(crate) fn schedule(&self, _reason: PollReason) {
        if SERVICE.is_inited() {
            self.start();
        }
        if self.schedule_prep() {
            self.waiters.wake();
        }
    }

    pub(crate) fn register_tx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.tx_waiters)
    }

    pub(crate) fn assist_once(&self) {
        self.schedule(PollReason::Assist);
        if self.poll_scheduled_once(self.assist_budget) {
            self.waiters.wake();
        }
    }

    fn run_task(&self) {
        let mut registrations = PollRegistrations::new();
        ktask::future::block_on(poll_fn(move |cx| {
            self.poll_background_rounds();

            let mut context = registrations.context(cx);
            if context.register(&self.waiters).is_err() {
                context.wake_by_ref();
                return Poll::<()>::Pending;
            }
            drop(context);

            self.poll_background_rounds();
            Poll::<()>::Pending
        }));
    }

    fn schedule_prep(&self) -> bool {
        if self.scheduled.swap(true, Ordering::AcqRel) {
            self.missed.store(true, Ordering::Release);
            return false;
        }

        true
    }

    fn poll_background_rounds(&self) {
        for _ in 0..self.background_max_rounds {
            if !self.poll_scheduled_once(self.background_budget) {
                return;
            }
        }
        self.waiters.wake();
    }

    fn poll_scheduled_once(&self, budget: PollBudget) -> bool {
        if !self.scheduled.load(Ordering::Acquire) {
            return false;
        }
        if self.active.swap(true, Ordering::AcqRel) {
            return true;
        }

        let progress = self.run_once(budget);
        let should_repoll = self.complete(progress);
        self.active.store(false, Ordering::Release);
        should_repoll
    }

    fn run_once(&self, budget: PollBudget) -> PollProgress {
        if !SERVICE.is_inited() {
            return PollProgress::default();
        }

        let progress = SERVICE.poll_budgeted(budget);
        if progress.tx_packets > 0 {
            self.tx_waiters.wake();
        }
        let _ = (progress.rx_packets, progress.timer_events);
        progress
    }

    fn complete(&self, progress: PollProgress) -> bool {
        if progress.has_more {
            return true;
        }

        if self.missed.swap(false, Ordering::AcqRel) {
            return true;
        }

        self.scheduled.store(false, Ordering::Release);
        if self.missed.swap(false, Ordering::AcqRel) {
            self.scheduled.store(true, Ordering::Release);
            return true;
        }

        false
    }
}

pub(crate) fn network_poller() -> &'static NetworkPoller {
    &NETWORK_POLLER
}

fn poller_task() {
    NETWORK_POLLER.run_task();
}
