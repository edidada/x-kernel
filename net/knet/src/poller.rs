// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Budgeted network data-plane poller.

use core::sync::atomic::{AtomicU8, Ordering};

use bitflags::bitflags;
use klazy::lazy_static;
use kpoll::{PollContext, PollRegisterError, PollSet};

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

const BACKGROUND_MAX_ROUNDS: usize = 4;

lazy_static! {
    static ref NETWORK_POLLER: NetworkPoller = NetworkPoller::new(NetworkPollerConfig {
        background_budget: DEFAULT_BACKGROUND_BUDGET,
        assist_budget: DEFAULT_ASSIST_BUDGET,
    });
}

bitflags! {
    /// Work classes that require data-plane advancement.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub(crate) struct AdvanceReason: u8 {
        /// Device or protocol ingress is ready.
        const RX = 1 << 0;
        /// Socket or device egress is ready.
        const TX = 1 << 1;
        /// A protocol or deferred-close timer has expired.
        const TIMER = 1 << 2;
    }
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
    /// Typed work that remains ready for another immediate round.
    ///
    /// A future protocol timer deadline does not count as immediate work.
    pub more: AdvanceReason,
}

pub(crate) struct NetworkPollerConfig {
    background_budget: PollBudget,
    assist_budget: PollBudget,
}

pub(crate) struct NetworkPoller {
    tx_waiters: PollSet,
    /// Work classes published before the corresponding kwork notification.
    ///
    /// Producers use `Release` before queueing work, and the executor uses an
    /// `AcqRel` swap so the acquired reasons also synchronize prior data-plane
    /// updates with the poll round.
    pending_reasons: AtomicU8,
    poller: kwork::BudgetedPoller<PollBudget, fn(PollBudget) -> kwork::BudgetedPollProgress>,
}

impl NetworkPoller {
    fn new(config: NetworkPollerConfig) -> Self {
        Self {
            tx_waiters: PollSet::new(),
            pending_reasons: AtomicU8::new(0),
            poller: kwork::BudgetedPoller::new(
                "knet-poller",
                config.background_budget,
                config.assist_budget,
                BACKGROUND_MAX_ROUNDS,
                poll_network_once,
            ),
        }
    }

    /// Starts the background workqueue during network initialization.
    pub(crate) fn start(&self) {
        if let Err(err) = self.poller.start() {
            warn!("failed to start knet-poller workqueue: {err:?}");
        }
    }

    /// Publishes network work and queues the background worker when it is idle.
    ///
    /// This method may run in IRQ-adjacent paths and does not acquire a sleepable
    /// lock or execute the network data plane.
    pub(crate) fn notify(&self, reason: AdvanceReason) {
        if reason.is_empty() {
            return;
        }
        // Publish reasons before the kwork state transition so an executor
        // acquiring scheduled work observes the matching data-plane updates.
        self.pending_reasons
            .fetch_or(reason.bits(), Ordering::Release);
        let _ = self.poller.notify_irq_safe();
    }

    pub(crate) fn register_tx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.tx_waiters)
    }

    /// Assists one already scheduled round from the current task context.
    pub(crate) fn assist_once(&self) {
        self.poller.assist_once();
    }

    fn run_once(&self, budget: PollBudget) -> PollProgress {
        if !SERVICE.is_inited() {
            return PollProgress::default();
        }

        let reasons =
            AdvanceReason::from_bits_truncate(self.pending_reasons.swap(0, Ordering::AcqRel));
        let progress = SERVICE.poll_budgeted(budget, reasons);
        self.pending_reasons
            .fetch_or(progress.more.bits(), Ordering::Release);
        if progress.tx_capacity_changed {
            self.tx_waiters.wake();
        }
        debug_assert!(progress.rx_packets <= budget.rx_packets);
        debug_assert!(progress.tx_packets <= budget.tx_packets);
        debug_assert!(progress.timer_events <= budget.timer_events);
        progress
    }
}

pub(crate) fn network_poller() -> &'static NetworkPoller {
    &NETWORK_POLLER
}

/// Assists one already scheduled network polling round from task context.
///
/// The call does not publish new work or wait for another executor.
pub(crate) fn assist_once() {
    network_poller().assist_once();
}

fn poll_network_once(budget: PollBudget) -> kwork::BudgetedPollProgress {
    kwork::BudgetedPollProgress {
        has_more: !NETWORK_POLLER.run_once(budget).more.is_empty(),
    }
}
