// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Serialized IPv4 admission, fragment reassembly, and deadline publication.

mod admission;
mod timer;

use alloc::vec::Vec;

pub(crate) use admission::AdmittedIpv4Packet;
use ksync::{Mutex, MutexGuard};
use ktime_types::MonotonicInstant;

use self::{admission::Ipv4Admission, timer::ReassemblyTimer};
use crate::{buf::PacketBuf, ip::Ipv4Cidr, network::fragment::ExpiredIpv4Fragment};

#[derive(Default)]
pub(crate) struct Ipv4ExpirationProgress {
    pub(crate) timer_events: usize,
    pub(crate) has_more: bool,
    pub(crate) fragments: Vec<ExpiredIpv4Fragment>,
}

/// Owns IPv4 input state and the timer sampled by the runtime.
pub(crate) struct Ipv4Ingress {
    state: Mutex<Ipv4Admission>,
    reassembly_timer: ReassemblyTimer,
}

impl Ipv4Ingress {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(Ipv4Admission::new()),
            reassembly_timer: ReassemblyTimer::new(),
        }
    }

    /// Starts a non-blocking IPv4 admission phase in task context.
    ///
    /// Returns `None` on contention. Keep packet delivery, socket callbacks, and
    /// response construction outside the guard's scope. Dropping the guard
    /// publishes the next reassembly deadline before releasing the state lock.
    pub(crate) fn try_admission(&self) -> Option<Ipv4AdmissionGuard<'_>> {
        Some(Ipv4AdmissionGuard {
            state: self.state.try_lock()?,
            timer: &self.reassembly_timer,
        })
    }

    pub(crate) fn set_ipv4_addrs(&self, addrs: &[Ipv4Cidr]) {
        self.state.lock().update_local_ipv4_addrs(addrs);
    }

    /// Marks due fragment work without taking the input-state lock.
    pub(crate) fn handle_timer_tick(&self, current_micros: u64) -> bool {
        self.reassembly_timer.handle_timer_tick(current_micros)
    }

    #[cfg(unittest)]
    pub(crate) fn set_reassembly_deadline_for_test(&self, next: MonotonicInstant) {
        self.reassembly_timer.update_reassembly_deadline(Some(next));
    }

    #[cfg(unittest)]
    pub(crate) fn with_state_locked_for_test<R>(&self, action: impl FnOnce() -> R) -> R {
        let _state = self.state.lock();
        action()
    }

    pub(crate) fn has_pending_timer(&self) -> bool {
        self.reassembly_timer.has_pending_timer()
    }

    /// Detaches expired queues and publishes timer state under the input lock.
    ///
    /// The caller constructs responses from the returned fragments after this
    /// function releases the lock. Contention retains pending timer work.
    pub(crate) fn expire_fragments(
        &self,
        timestamp: MonotonicInstant,
        timer_budget: usize,
    ) -> Ipv4ExpirationProgress {
        if !self.has_pending_timer() {
            return Ipv4ExpirationProgress::default();
        }
        let Some(mut ingress) = self.state.try_lock() else {
            return Ipv4ExpirationProgress {
                timer_events: 0,
                has_more: true,
                ..Ipv4ExpirationProgress::default()
            };
        };
        let expired = ingress.expire_fragments(timestamp, timer_budget);
        let next_deadline = ingress.reassembly_deadline();
        let has_more = next_deadline.is_some_and(|deadline| deadline <= timestamp);
        self.reassembly_timer
            .complete_expiration(next_deadline, has_more);
        drop(ingress);
        Ipv4ExpirationProgress {
            timer_events: expired.timer_events,
            has_more: self.has_pending_timer(),
            fragments: expired.fragments,
        }
    }
}

/// Restricts a locked input phase to IPv4 admission and deadline publication.
pub(crate) struct Ipv4AdmissionGuard<'a> {
    state: MutexGuard<'a, Ipv4Admission>,
    timer: &'a ReassemblyTimer,
}

impl Ipv4AdmissionGuard<'_> {
    /// Validates local input and completes reassembly while holding the state lock.
    pub(crate) fn admit_packet(
        &mut self,
        timestamp: MonotonicInstant,
        packet: PacketBuf,
    ) -> Option<AdmittedIpv4Packet> {
        self.state.admit_packet(timestamp, packet)
    }
}

impl Drop for Ipv4AdmissionGuard<'_> {
    fn drop(&mut self) {
        self.timer
            .update_reassembly_deadline(self.state.reassembly_deadline());
    }
}
