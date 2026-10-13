// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 input processing, fragment reassembly state, and expiration tracking.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use khal::time::monotonic_time;
use ksync::Mutex;
use ktime_types::MonotonicInstant;

use crate::{
    buf::PacketBuf,
    ip::{Ipv4Address, Ipv4Cidr},
    network::{
        fragment::{Ipv4Reassembler, Ipv4ReassemblyResult},
        ipv4::{self, Icmpv4Error, Ipv4Error, Ipv4Header},
    },
    udp, udp_err,
};

const IPV4_REASSEMBLY_TIMER_PENDING: u64 = 1;
const IPV4_REASSEMBLY_DEADLINE_OFFSET: u64 = 2;

#[derive(Default)]
pub(crate) struct Ipv4ExpirationProgress {
    pub(crate) timer_events: usize,
    pub(crate) has_more: bool,
}

/// Owns IPv4 input state and the timer sampled by the runtime.
pub(crate) struct Ipv4Ingress {
    state: Mutex<Ipv4IngressState>,
    reassembly_deadline_micros: AtomicU64,
}

impl Ipv4Ingress {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(Ipv4IngressState::new()),
            reassembly_deadline_micros: AtomicU64::new(0),
        }
    }

    /// Processes a received IP batch and publishes its reassembly deadline under one lock.
    ///
    /// IPv6 packets pass through to the shared stack. Returns `false` on input-state
    /// contention, retaining the entire batch for retry.
    pub(crate) fn try_process_rx_batch(
        &self,
        device_timestamp: MonotonicInstant,
        rx_batch: &mut Vec<PacketBuf>,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> bool {
        if rx_batch.is_empty() {
            return true;
        }

        let Some(mut ingress) = self.state.try_lock() else {
            return false;
        };
        for packet in rx_batch.drain(..) {
            let Some(ip_packet) = packet.network_packet() else {
                continue;
            };
            match crate::ipv4::ip_version(ip_packet) {
                Some(4) => ingress.process_packet(
                    device_timestamp,
                    packet,
                    accepted_packets,
                    control_packets,
                ),
                Some(6) => accepted_packets.push(packet),
                _ => debug!("Dropping packet with invalid IP version"),
            }
        }
        let next_deadline = ingress.ipv4_reassembly_deadline();
        self.update_reassembly_deadline(next_deadline);
        drop(ingress);
        true
    }

    pub(crate) fn set_ipv4_addrs(&self, addrs: &[Ipv4Cidr]) {
        self.state.lock().update_local_ipv4_addrs(addrs);
    }

    /// Marks due fragment work without taking the input-state lock.
    pub(crate) fn handle_timer_tick(&self, current_micros: u64) -> bool {
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

    fn update_reassembly_deadline(&self, next: Option<MonotonicInstant>) {
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

    #[cfg(unittest)]
    pub(crate) fn set_reassembly_deadline_for_test(&self, next: MonotonicInstant) {
        self.update_reassembly_deadline(Some(next));
    }

    #[cfg(unittest)]
    pub(crate) fn with_state_locked_for_test<R>(&self, action: impl FnOnce() -> R) -> R {
        let _state = self.state.lock();
        action()
    }

    fn complete_expiration(&self, next: Option<MonotonicInstant>, has_more: bool) {
        let next_state = if has_more {
            IPV4_REASSEMBLY_TIMER_PENDING
        } else {
            ipv4_reassembly_timer_state(next)
        };
        self.reassembly_deadline_micros
            .store(next_state, Ordering::Release);
    }

    pub(crate) fn has_pending_timer(&self) -> bool {
        self.reassembly_deadline_micros.load(Ordering::Acquire) == IPV4_REASSEMBLY_TIMER_PENDING
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
        let timer_events = ingress.expire_ipv4_fragments(timestamp, timer_budget, control_packets);
        let next_deadline = ingress.ipv4_reassembly_deadline();
        let has_more = next_deadline.is_some_and(|deadline| deadline <= timestamp);
        self.complete_expiration(next_deadline, has_more);
        drop(ingress);
        Ipv4ExpirationProgress {
            timer_events,
            has_more: self.has_pending_timer(),
        }
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

struct Ipv4IngressState {
    local_ipv4_addrs: Vec<Ipv4Cidr>,
    ipv4_reassembler: Ipv4Reassembler,
}

impl Ipv4IngressState {
    fn new() -> Self {
        Self {
            local_ipv4_addrs: Vec::new(),
            ipv4_reassembler: Ipv4Reassembler::new(),
        }
    }

    fn update_local_ipv4_addrs(&mut self, addrs: &[Ipv4Cidr]) {
        self.local_ipv4_addrs.clear();
        self.local_ipv4_addrs.extend_from_slice(addrs);
    }

    fn ipv4_reassembly_deadline(&self) -> Option<MonotonicInstant> {
        self.ipv4_reassembler.expiration_deadline()
    }

    fn expire_ipv4_fragments(
        &mut self,
        timestamp: MonotonicInstant,
        timer_budget: usize,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> usize {
        let expired = self
            .ipv4_reassembler
            .remove_expired(timestamp, timer_budget);
        for fragment in expired.fragments {
            queue_icmpv4_error(
                control_packets,
                Icmpv4Error::FragmentReassemblyTimeout,
                fragment.packet_type,
                fragment.header,
                &fragment.packet,
            );
        }
        expired.timer_events
    }

    /// Processes IPv4 input before accepted packets enter the shared stack.
    fn process_packet(
        &mut self,
        timestamp: MonotonicInstant,
        mut packet: PacketBuf,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) {
        let header = match Ipv4Header::validate_input_packet(&mut packet) {
            Ok(header) => header,
            Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return,
        };

        if !self.is_local_ipv4_destination(header.dst_addr()) {
            return;
        }

        if header.is_fragmented() {
            match self.ipv4_reassembler.reassemble(packet, header, timestamp) {
                Ipv4ReassemblyResult::Complete(packet) => {
                    self.process_packet(timestamp, packet, accepted_packets, control_packets)
                }
                Ipv4ReassemblyResult::Pending | Ipv4ReassemblyResult::Dropped => {}
            }
            return;
        }

        if header.protocol() == ipv4::PROTOCOL_UDP {
            let packet_type = packet.packet_type();
            let packet = match udp::prepare_ipv4_packet(header, packet) {
                Ok(packet) => packet,
                Err(_) => return,
            };
            let (disposition, returned_packet) = udp::deliver_ipv4_packet(packet);
            match disposition {
                udp::InputDisposition::Accepted => return,
                udp::InputDisposition::NoSocket if !header.is_broadcast_or_multicast() => {
                    let Some(packet) = returned_packet else {
                        return;
                    };
                    let Some(ip_packet) = packet.packet().network_packet() else {
                        return;
                    };
                    queue_icmpv4_error(
                        control_packets,
                        Icmpv4Error::PortUnreachable,
                        packet_type,
                        header,
                        ip_packet,
                    );
                    return;
                }
                udp::InputDisposition::NoSocket => return,
            }
        }

        if should_emit_protocol_unreachable(header) {
            if let Some(ip_packet) = packet.network_packet() {
                queue_icmpv4_error(
                    control_packets,
                    Icmpv4Error::ProtocolUnreachable,
                    packet.packet_type(),
                    header,
                    ip_packet,
                );
            }
            return;
        }

        if let Some(ip_packet) = packet.network_packet() {
            udp_err::inspect_icmpv4_error(ip_packet, packet.ifindex());
        }
        accepted_packets.push(packet);
    }

    fn is_local_ipv4_destination(&self, dst_addr: Ipv4Address) -> bool {
        dst_addr.is_broadcast()
            || dst_addr.is_multicast()
            || self
                .local_ipv4_addrs
                .iter()
                .any(|addr| addr.address() == dst_addr || addr.broadcast() == Some(dst_addr))
    }
}

fn queue_icmpv4_error(
    control_packets: &mut Vec<Vec<u8>>,
    error: Icmpv4Error,
    packet_type: crate::buf::PacketType,
    offending_header: Ipv4Header,
    offending_packet: &[u8],
) {
    let Some(packet) =
        ipv4::build_icmpv4_error_packet(error, packet_type, offending_header, offending_packet)
    else {
        return;
    };
    control_packets.push(packet);
}

fn should_emit_protocol_unreachable(header: Ipv4Header) -> bool {
    !header.is_fragmented()
        && !header.is_broadcast_or_multicast()
        && !matches!(
            header.protocol(),
            ipv4::PROTOCOL_TCP | ipv4::PROTOCOL_UDP | ipv4::PROTOCOL_ICMP
        )
}

#[unittest::mod_test]
mod tests {
    use etherparse::{IpNumber, Ipv4Header as EtherIpv4Header};
    use ktime_types::TimeSpan;
    use unittest::def_test;

    use super::*;
    use crate::buf::PacketOwner;

    #[def_test]
    fn received_fragment_publishes_expiration_deadline() {
        let ingress = Ipv4Ingress::new();
        ingress.set_ipv4_addrs(&[Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 2), 24).unwrap()]);
        let payload = b"abcdefgh";
        let mut header = EtherIpv4Header::new(
            payload.len() as u16,
            64,
            IpNumber(ipv4::PROTOCOL_UDP),
            [192, 0, 2, 1],
            [192, 0, 2, 2],
        )
        .unwrap();
        header.dont_fragment = false;
        header.more_fragments = true;
        header.header_checksum = header.calc_header_checksum();
        let mut bytes = header.to_bytes().to_vec();
        bytes.extend_from_slice(payload);
        let mut rx_batch = alloc::vec![PacketBuf::from_ip_packet_vec(
            1,
            bytes,
            PacketOwner::DeviceRx,
        )];
        let mut accepted_packets = Vec::new();
        let mut control_packets = Vec::new();
        let timestamp = monotonic_time();

        assert!(ingress.try_process_rx_batch(
            timestamp,
            &mut rx_batch,
            &mut accepted_packets,
            &mut control_packets,
        ));
        assert!(rx_batch.is_empty());
        assert!(accepted_packets.is_empty());
        assert!(control_packets.is_empty());
        let after_expiration = timestamp + TimeSpan::from_secs(60);
        assert!(ingress.handle_timer_tick(
            u64::try_from(after_expiration.span_since_origin().as_micros()).unwrap()
        ));
        let progress = ingress.expire_fragments(after_expiration, 1, &mut control_packets);
        assert_eq!(progress.timer_events, 1);
        assert!(!progress.has_more);
        assert_eq!(control_packets.len(), 1);
        assert!(!ingress.has_pending_timer());
    }

    #[def_test]
    fn contended_input_retains_batch_for_retry() {
        let ingress = Ipv4Ingress::new();
        let mut rx_batch = alloc::vec![PacketBuf::from_ip_packet_vec(
            1,
            alloc::vec![0x60; 40],
            PacketOwner::DeviceRx,
        )];
        let mut accepted_packets = Vec::new();
        let mut control_packets = Vec::new();
        let timestamp = monotonic_time();
        let was_processed = ingress.with_state_locked_for_test(|| {
            ingress.try_process_rx_batch(
                timestamp,
                &mut rx_batch,
                &mut accepted_packets,
                &mut control_packets,
            )
        });
        assert!(!was_processed);
        assert_eq!(rx_batch.len(), 1);
        assert!(accepted_packets.is_empty());
        assert!(control_packets.is_empty());
        assert!(ingress.try_process_rx_batch(
            timestamp,
            &mut rx_batch,
            &mut accepted_packets,
            &mut control_packets,
        ));
        assert!(rx_batch.is_empty());
        assert_eq!(accepted_packets.len(), 1);
    }
}
