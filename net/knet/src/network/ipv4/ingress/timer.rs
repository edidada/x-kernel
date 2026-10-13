// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Atomic publication of fragment deadlines between input processing and timer ticks.

use core::sync::atomic::{AtomicU64, Ordering};

use khal::time::monotonic_time;
use ktime_types::MonotonicInstant;

const IPV4_REASSEMBLY_TIMER_PENDING: u64 = 1;
const IPV4_REASSEMBLY_DEADLINE_OFFSET: u64 = 2;

/// Encodes a disarmed timer, pending work, or a future deadline.
///
/// Input processing serializes publication and completion under its state lock;
/// timer ticks only mark due work through the atomic state.
pub(super) struct ReassemblyTimer {
    reassembly_deadline_micros: AtomicU64,
}

impl ReassemblyTimer {
    pub(super) fn new() -> Self {
        Self {
            reassembly_deadline_micros: AtomicU64::new(0),
        }
    }

    /// Marks due fragment work without taking the input-state lock.
    pub(super) fn handle_timer_tick(&self, current_micros: u64) -> bool {
        let reassembly_deadline = self.reassembly_deadline_micros.load(Ordering::Acquire);
        reassembly_deadline > IPV4_REASSEMBLY_TIMER_PENDING
            && current_micros.saturating_add(IPV4_REASSEMBLY_DEADLINE_OFFSET) >= reassembly_deadline
            && self
                .reassembly_deadline_micros
                .compare_exchange(
                    reassembly_deadline,
                    IPV4_REASSEMBLY_TIMER_PENDING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
    }

    /// Publishes the next deadline while the caller holds the input-state lock.
    pub(super) fn update_reassembly_deadline(&self, next: Option<MonotonicInstant>) {
        let next_state = ipv4_reassembly_timer_state(next);
        let mut current = self.reassembly_deadline_micros.load(Ordering::Acquire);
        loop {
            // A timer tick owns the pending state until the poller consumes
            // the due queues. A concurrent RX refresh must not disarm it.
            if current == IPV4_REASSEMBLY_TIMER_PENDING {
                return;
            }
            match self.reassembly_deadline_micros.compare_exchange_weak(
                current,
                next_state,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Consumes pending expiration work while the caller holds the input-state lock.
    pub(super) fn complete_expiration(&self, next: Option<MonotonicInstant>, has_more: bool) {
        let next_state = if has_more {
            IPV4_REASSEMBLY_TIMER_PENDING
        } else {
            ipv4_reassembly_timer_state(next)
        };
        self.reassembly_deadline_micros
            .store(next_state, Ordering::Release);
    }

    pub(super) fn has_pending_timer(&self) -> bool {
        self.reassembly_deadline_micros.load(Ordering::Acquire) == IPV4_REASSEMBLY_TIMER_PENDING
    }
}

fn ipv4_reassembly_timer_state(next: Option<MonotonicInstant>) -> u64 {
    let Some(next) = next else {
        return 0;
    };
    if next <= monotonic_time() {
        return IPV4_REASSEMBLY_TIMER_PENDING;
    }
    u64::try_from(next.span_since_origin().as_micros())
        .unwrap_or(u64::MAX)
        .saturating_add(IPV4_REASSEMBLY_DEADLINE_OFFSET)
}
