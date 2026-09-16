// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-round poller progress and reschedule reasons.
//!
//! [`PollRound`] stores one round's shared counters, budgets, deadline, and
//! pending-work flags. Module-specific progress types stay in their owners;
//! [`Service`](super::service::Service) maps those results into this context.
use ktime_types::MonotonicInstant;
use smoltcp::time::Instant as SmoltcpInstant;

use crate::poller::{PollBudget, PollProgress};

/// Outcome of a poll stage that may stop the rest of the round.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PollStageResult {
    Complete,
    Deferred,
}

bitflags::bitflags! {
    /// Reasons a normally completed round should be followed by another immediately.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub(crate) struct PendingWork: u16 {
        const TX_CONTENTION = 1 << 0;
        const RX_DEFERRED = 1 << 1;
        const QUEUE_SNAPSHOT_CONTENTION = 1 << 2;
        const INTERFACE_SNAPSHOT_CONTENTION = 1 << 3;
        const DEVICE_RX_READY = 1 << 4;
        const INGRESS_QUEUE = 1 << 5;
        const TX_QUEUE = 1 << 6;
        const DATAPLANE_TX = 1 << 7;
        const PROTOCOL_TIMER = 1 << 8;
        /// The IPv4 expiration stage reported remaining work.
        const IPV4_REASSEMBLY_WORK = 1 << 9;
        /// The IPv4 timer is pending at the end-of-round snapshot.
        const IPV4_REASSEMBLY_TIMER = 1 << 10;
    }
}

/// Mutable progress for one [`Service::poll_budgeted`](super::service::Service::poll_budgeted) round.
pub(crate) struct PollRound {
    pub budget: PollBudget,
    pub started_at: MonotonicInstant,
    rx_packets: usize,
    tx_packets: usize,
    timer_events: usize,
    reached_time_limit: bool,
    tx_capacity_changed: bool,
    next_poll_deadline: Option<SmoltcpInstant>,
    pending: PendingWork,
}

impl PollRound {
    pub(crate) fn new(budget: PollBudget, started_at: MonotonicInstant) -> Self {
        Self {
            budget,
            started_at,
            rx_packets: 0,
            tx_packets: 0,
            timer_events: 0,
            reached_time_limit: false,
            tx_capacity_changed: false,
            next_poll_deadline: None,
            pending: PendingWork::empty(),
        }
    }

    pub(crate) fn reached_time_limit(&self) -> bool {
        self.reached_time_limit
    }

    pub(crate) fn add_rx_packets(&mut self, packets: usize) {
        self.rx_packets += packets;
    }

    pub(crate) fn add_tx_packets(&mut self, packets: usize) {
        self.tx_packets += packets;
    }

    pub(crate) fn add_timer_events(&mut self, events: usize) {
        self.timer_events += events;
    }

    pub(crate) fn can_record_timer_event(&self) -> bool {
        self.timer_events < self.budget.timer_events
    }

    pub(crate) fn reach_time_limit(&mut self) {
        self.reached_time_limit = true;
    }

    pub(crate) fn set_reached_time_limit(&mut self, reached: bool) {
        self.reached_time_limit = reached;
    }

    pub(crate) fn mark_tx_capacity_changed(&mut self) {
        self.tx_capacity_changed = true;
    }

    pub(crate) fn mark_pending(&mut self, pending: PendingWork) {
        self.pending.insert(pending);
    }

    pub(crate) fn set_next_poll_deadline(&mut self, deadline: Option<SmoltcpInstant>) {
        self.next_poll_deadline = deadline;
    }

    pub(crate) fn remaining_rx(&self) -> usize {
        if self.reached_time_limit || self.is_rx_deferred() {
            return 0;
        }
        self.budget.rx_packets.saturating_sub(self.rx_packets)
    }

    pub(crate) fn remaining_tx(&self) -> usize {
        if self.reached_time_limit || self.pending.contains(PendingWork::TX_CONTENTION) {
            return 0;
        }
        self.budget.tx_packets.saturating_sub(self.tx_packets)
    }

    pub(crate) fn is_rx_deferred(&self) -> bool {
        self.pending.contains(PendingWork::RX_DEFERRED)
    }

    pub(crate) fn next_poll_deadline(&self) -> Option<SmoltcpInstant> {
        self.next_poll_deadline
    }

    /// Preserves completed work and requests a retry without readiness snapshots.
    pub(crate) fn into_deferred_progress(self) -> PollProgress {
        PollProgress {
            rx_packets: self.rx_packets,
            tx_packets: self.tx_packets,
            timer_events: self.timer_events,
            tx_capacity_changed: self.tx_capacity_changed,
            has_more: true,
        }
    }

    pub(crate) fn into_progress(self) -> PollProgress {
        PollProgress {
            rx_packets: self.rx_packets,
            tx_packets: self.tx_packets,
            timer_events: self.timer_events,
            tx_capacity_changed: self.tx_capacity_changed,
            has_more: !self.pending.is_empty(),
        }
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    const TEST_BUDGET: PollBudget = PollBudget {
        rx_packets: 4,
        tx_packets: 3,
        timer_events: 2,
    };

    #[def_test]
    fn front_and_tail_stages_share_packet_budgets() {
        let mut round = PollRound::new(TEST_BUDGET, MonotonicInstant::ORIGIN);
        round.add_tx_packets(1);
        round.mark_tx_capacity_changed();
        round.add_rx_packets(2);
        assert_eq!(round.remaining_tx(), 2);
        assert_eq!(round.remaining_rx(), 2);

        round.add_tx_packets(2);
        round.add_rx_packets(2);
        assert_eq!(round.remaining_tx(), 0);
        assert_eq!(round.remaining_rx(), 0);
        let progress = round.into_progress();
        assert_eq!(progress.tx_packets, 3);
        assert_eq!(progress.rx_packets, 4);
        assert!(progress.tx_capacity_changed);
        assert!(!progress.has_more);
    }

    #[def_test]
    fn contention_pauses_only_the_affected_device_stage() {
        let mut tx_round = PollRound::new(TEST_BUDGET, MonotonicInstant::ORIGIN);
        tx_round.mark_pending(PendingWork::TX_CONTENTION);
        assert_eq!(tx_round.remaining_tx(), 0);
        assert_eq!(tx_round.remaining_rx(), TEST_BUDGET.rx_packets);
        assert!(tx_round.into_progress().has_more);

        let mut rx_round = PollRound::new(TEST_BUDGET, MonotonicInstant::ORIGIN);
        rx_round.mark_pending(PendingWork::RX_DEFERRED);
        assert_eq!(rx_round.remaining_rx(), 0);
        assert_eq!(rx_round.remaining_tx(), TEST_BUDGET.tx_packets);
        assert!(rx_round.into_progress().has_more);
    }

    #[def_test]
    fn stack_can_clear_the_round_time_limit() {
        let mut round = PollRound::new(TEST_BUDGET, MonotonicInstant::ORIGIN);
        round.reach_time_limit();
        assert_eq!(round.remaining_tx(), 0);
        assert_eq!(round.remaining_rx(), 0);

        round.set_reached_time_limit(false);
        assert!(!round.reached_time_limit());
        assert_eq!(round.remaining_tx(), TEST_BUDGET.tx_packets);
        assert_eq!(round.remaining_rx(), TEST_BUDGET.rx_packets);
    }

    #[def_test]
    fn timer_events_and_deadline_follow_generic_updates() {
        let _protocol = SmoltcpInstant::from_millis(20);
        let close = SmoltcpInstant::from_millis(10);
        for (timer_budget, expired, expected) in [(0, 0, 0), (2, 1, 2), (2, 2, 2)] {
            let mut round = PollRound::new(
                PollBudget {
                    timer_events: timer_budget,
                    ..TEST_BUDGET
                },
                MonotonicInstant::ORIGIN,
            );
            round.add_timer_events(expired);
            if round.can_record_timer_event() {
                round.add_timer_events(1);
            }
            round.set_next_poll_deadline(Some(close));
            assert_eq!(round.next_poll_deadline(), Some(close));
            let progress = round.into_progress();
            assert_eq!(progress.timer_events, expected);
            assert!(!progress.has_more);
        }
    }

    #[def_test]
    fn deferred_round_preserves_progress_and_always_requests_retry() {
        let mut round = PollRound::new(TEST_BUDGET, MonotonicInstant::ORIGIN);
        round.add_tx_packets(2);
        round.mark_tx_capacity_changed();
        round.add_rx_packets(1);
        round.add_timer_events(1);

        let progress = round.into_deferred_progress();
        assert_eq!(progress.tx_packets, 2);
        assert_eq!(progress.rx_packets, 1);
        assert_eq!(progress.timer_events, 1);
        assert!(progress.tx_capacity_changed);
        assert!(progress.has_more);
    }
}
