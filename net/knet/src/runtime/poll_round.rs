// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-round budgets, progress, deadlines, and reschedule reasons.
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
