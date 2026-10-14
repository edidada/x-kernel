// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ-safe publication of fragment deadlines between input processing and timer ticks.

use khal::time::monotonic_time;
use kspin::SpinNoIrq;
use ktime_types::MonotonicInstant;

#[derive(Clone, Copy)]
enum ReassemblyTimerState {
    Disarmed,
    Pending,
    Armed { deadline: MonotonicInstant },
}

impl ReassemblyTimerState {
    fn from_deadline(next: Option<MonotonicInstant>) -> Self {
        match next {
            None => Self::Disarmed,
            Some(deadline) if deadline <= monotonic_time() => Self::Pending,
            Some(deadline) => Self::Armed { deadline },
        }
    }
}

/// Shares reassembly timer state with IRQ sampling through a short critical section.
///
/// Publication and completion take the ingress state lock before this lock.
/// Timer sampling takes only this lock. No queue processing, allocation, or
/// wakeup runs while it is held.
pub(super) struct ReassemblyTimer {
    state: SpinNoIrq<ReassemblyTimerState>,
}

impl ReassemblyTimer {
    pub(super) fn new() -> Self {
        Self {
            state: SpinNoIrq::new(ReassemblyTimerState::Disarmed),
        }
    }

    /// Marks due work from task or IRQ context without taking the ingress lock.
    pub(super) fn handle_timer_tick(&self, current_micros: u64) -> bool {
        let mut state = self.state.lock();
        match *state {
            ReassemblyTimerState::Armed { deadline }
                if deadline.span_since_origin().as_micros() <= u128::from(current_micros) =>
            {
                *state = ReassemblyTimerState::Pending;
                true
            }
            _ => false,
        }
    }

    /// Publishes the next deadline while the caller holds the ingress state lock.
    pub(super) fn update_reassembly_deadline(&self, next: Option<MonotonicInstant>) {
        let next_state = ReassemblyTimerState::from_deadline(next);
        let mut state = self.state.lock();
        // RX must leave pending work for the poller to consume.
        if !matches!(*state, ReassemblyTimerState::Pending) {
            *state = next_state;
        }
    }

    /// Completes expiration while the caller holds the ingress state lock.
    pub(super) fn complete_expiration(&self, next: Option<MonotonicInstant>, has_more: bool) {
        let next_state = if has_more {
            ReassemblyTimerState::Pending
        } else {
            ReassemblyTimerState::from_deadline(next)
        };
        *self.state.lock() = next_state;
    }

    pub(super) fn has_pending_timer(&self) -> bool {
        matches!(*self.state.lock(), ReassemblyTimerState::Pending)
    }
}
