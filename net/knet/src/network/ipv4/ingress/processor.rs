// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Coordinates admission, deadline publication, and delivery across the input lock.

use alloc::vec::Vec;

use ksync::Mutex;
use ktime_types::MonotonicInstant;

use super::{
    admission::Ipv4Admission,
    batch::{AdmittedIpPacket, IngressBatch},
    delivery::{deliver_batch, queue_icmpv4_error},
    timer::ReassemblyTimer,
};
use crate::{buf::PacketBuf, ip::Ipv4Cidr, network::ipv4::Icmpv4Error};

#[derive(Default)]
pub(crate) struct Ipv4ExpirationProgress {
    pub(crate) timer_events: usize,
    pub(crate) has_more: bool,
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

    /// Admits a batch and publishes its reassembly deadline under one input lock.
    ///
    /// Transport delivery and ICMP processing run after releasing the lock, in
    /// admission order. IPv6 packets pass through to the shared stack. Returns
    /// `false` on input-state contention, retaining the entire batch for retry.
    pub(crate) fn try_process_rx_batch(
        &self,
        device_timestamp: MonotonicInstant,
        rx_batch: &mut IngressBatch,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> bool {
        if rx_batch.packets.is_empty() {
            return true;
        }

        // Reserve outside the state lock. Runtime batches reuse this allocation.
        rx_batch.reserve_admitted();
        let Some(mut ingress) = self.state.try_lock() else {
            return false;
        };
        rx_batch.admit_packets(|packet| {
            let ip_packet = packet.network_packet()?;
            match crate::ipv4::ip_version(ip_packet) {
                Some(4) => ingress
                    .admit_packet(device_timestamp, packet)
                    .map(AdmittedIpPacket::Ipv4),
                Some(6) => Some(AdmittedIpPacket::Ipv6(packet)),
                _ => {
                    debug!("Dropping packet with invalid IP version");
                    None
                }
            }
        });
        let next_deadline = ingress.reassembly_deadline();
        self.reassembly_timer
            .update_reassembly_deadline(next_deadline);
        drop(ingress);
        deliver_batch(rx_batch, accepted_packets, control_packets);
        true
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

    pub(crate) fn expire_fragments(
        &self,
        timestamp: MonotonicInstant,
        timer_budget: usize,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> Ipv4ExpirationProgress {
        if !self.has_pending_timer() {
            return Ipv4ExpirationProgress::default();
        }
        let Some(mut ingress) = self.state.try_lock() else {
            return Ipv4ExpirationProgress {
                timer_events: 0,
                has_more: true,
            };
        };
        let expired = ingress.expire_fragments(timestamp, timer_budget);
        let next_deadline = ingress.reassembly_deadline();
        let has_more = next_deadline.is_some_and(|deadline| deadline <= timestamp);
        self.reassembly_timer
            .complete_expiration(next_deadline, has_more);
        drop(ingress);
        for fragment in expired.fragments {
            queue_icmpv4_error(
                control_packets,
                Icmpv4Error::FragmentReassemblyTimeout,
                fragment.packet_type,
                fragment.header,
                &fragment.packet,
            );
        }
        Ipv4ExpirationProgress {
            timer_events: expired.timer_events,
            has_more: self.has_pending_timer(),
        }
    }
}

#[unittest::mod_test]
mod tests {
    include!("tests.rs");
}
