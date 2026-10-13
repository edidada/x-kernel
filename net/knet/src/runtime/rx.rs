// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RX batch ownership, protocol dispatch, and transport/stack handoff in task context.

use alloc::vec::Vec;

use ktime_types::MonotonicInstant;

use crate::{
    buf::PacketBuf,
    network::ipv4::{
        self, Icmpv4Error, Ipv4Header,
        ingress::{AdmittedIpv4Packet, Ipv4ExpirationProgress, Ipv4Ingress},
    },
    udp, udp_err,
};

/// Reusable RX storage for admission under the input lock and delivery after release.
pub(super) struct IngressBatch {
    pub(super) packets: Vec<PacketBuf>,
    admitted: Vec<AdmittedIpPacket>,
}

impl IngressBatch {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            packets: Vec::with_capacity(capacity),
            admitted: Vec::with_capacity(capacity),
        }
    }
}

enum AdmittedIpPacket {
    Ipv4(AdmittedIpv4Packet),
    Ipv6(PacketBuf),
}

/// Admits the whole batch before delivering packets in input order.
///
/// Contention retains every raw packet for retry. Staging storage is reserved
/// before admission; all transport callbacks run after releasing its guard.
pub(super) fn try_process_rx_batch(
    ipv4_ingress: &Ipv4Ingress,
    device_timestamp: MonotonicInstant,
    rx_batch: &mut IngressBatch,
    accepted_packets: &mut Vec<PacketBuf>,
    control_packets: &mut Vec<Vec<u8>>,
) -> bool {
    if rx_batch.packets.is_empty() {
        return true;
    }

    // Reserve outside the state lock. Runtime batches reuse this allocation.
    rx_batch.admitted.reserve(rx_batch.packets.len());
    let Some(mut ingress) = ipv4_ingress.try_admission() else {
        return false;
    };
    for packet in rx_batch.packets.drain(..) {
        let Some(ip_packet) = packet.network_packet() else {
            continue;
        };
        let admitted = match crate::ipv4::ip_version(ip_packet) {
            Some(4) => ingress
                .admit_packet(device_timestamp, packet)
                .map(AdmittedIpPacket::Ipv4),
            Some(6) => Some(AdmittedIpPacket::Ipv6(packet)),
            _ => {
                debug!("Dropping packet with invalid IP version");
                None
            }
        };
        if let Some(packet) = admitted {
            rx_batch.admitted.push(packet);
        }
    }
    drop(ingress);
    for packet in rx_batch.admitted.drain(..) {
        match packet {
            AdmittedIpPacket::Ipv4(packet) => {
                deliver_ipv4_packet(packet, accepted_packets, control_packets);
            }
            AdmittedIpPacket::Ipv6(packet) => accepted_packets.push(packet),
        }
    }
    true
}

/// Queues responses from detached expired fragments outside the input lock.
pub(super) fn expire_fragments(
    ingress: &Ipv4Ingress,
    timestamp: MonotonicInstant,
    timer_budget: usize,
    control_packets: &mut Vec<Vec<u8>>,
) -> Ipv4ExpirationProgress {
    let mut progress = ingress.expire_fragments(timestamp, timer_budget);
    for fragment in progress.fragments.drain(..) {
        queue_icmpv4_error(
            control_packets,
            Icmpv4Error::FragmentReassemblyTimeout,
            fragment.packet_type,
            fragment.header,
            &fragment.packet,
        );
    }
    progress
}

/// Delivers an admitted IPv4 packet with no input-state lock held.
///
/// Socket locks, wakeups and ICMP allocation must stay outside the input lock.
fn deliver_ipv4_packet(
    packet: AdmittedIpv4Packet,
    accepted_packets: &mut Vec<PacketBuf>,
    control_packets: &mut Vec<Vec<u8>>,
) {
    let (packet, header) = packet.into_parts();
    match header.protocol() {
        ipv4::PROTOCOL_UDP => {
            deliver_udp_packet(packet, header, control_packets);
            return;
        }
        ipv4::PROTOCOL_ICMP => {
            if let Some(ip_packet) = packet.network_packet() {
                udp_err::inspect_icmpv4_error(ip_packet, packet.ifindex());
            }
        }
        ipv4::PROTOCOL_TCP => {}
        _ if !header.is_broadcast_or_multicast() => {
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
        _ => {}
    }
    accepted_packets.push(packet);
}

fn deliver_udp_packet(packet: PacketBuf, header: Ipv4Header, control_packets: &mut Vec<Vec<u8>>) {
    let packet_type = packet.packet_type();
    let packet = match udp::prepare_ipv4_packet(header, packet) {
        Ok(packet) => packet,
        Err(_) => return,
    };
    let (disposition, returned_packet) = udp::deliver_ipv4_packet(packet);
    match disposition {
        udp::InputDisposition::Accepted => (),
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
        }
        udp::InputDisposition::NoSocket => (),
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

#[unittest::mod_test]
mod tests {
    use alloc::{sync::Arc, task::Wake};
    use core::{
        net::SocketAddrV4,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::{Context, Waker},
    };

    use khal::time::monotonic_time;
    use kpoll::{IoEvents, PollRegistrations, Pollable};
    use ktime_types::TimeSpan;
    use unittest::def_test;

    use super::*;
    use crate::{
        SocketAddrEx, SocketOps,
        buf::PacketOwner,
        ip::{Ipv4Address, Ipv4Cidr},
        network::ipv4,
        udp,
    };

    struct IngressWakeProbe {
        ingress: Arc<Ipv4Ingress>,
        wakes: AtomicUsize,
        has_locked_wake: AtomicBool,
    }

    impl Wake for IngressWakeProbe {
        fn wake(self: Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::Relaxed);
            if self.ingress.try_admission().is_none() {
                self.has_locked_wake.store(true, Ordering::Relaxed);
            }
        }
    }

    fn input_packet(bytes: Vec<u8>) -> PacketBuf {
        PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx)
    }

    fn udp_packet(port: u16, payload: &[u8]) -> Vec<u8> {
        let mut udp = etherparse::UdpHeader::without_ipv4_checksum(4321, port, payload.len())
            .unwrap()
            .to_bytes()
            .to_vec();
        udp.extend_from_slice(payload);
        ipv4::build_ipv4_packet(
            Ipv4Address::new(192, 0, 2, 1),
            Ipv4Address::new(192, 0, 2, 2),
            ipv4::PROTOCOL_UDP,
            64,
            &udp,
        )
        .unwrap()
    }

    // Serial: the socket owns a shared UDP registry entry and unregisters on Drop.
    #[def_test(serial)]
    fn udp_delivery_releases_input_lock() {
        let ingress = Arc::new(Ipv4Ingress::new());
        let local_addr = Ipv4Address::new(192, 0, 2, 2);
        ingress.set_ipv4_addrs(&[Ipv4Cidr::new(local_addr, 24).unwrap()]);
        let socket = udp::UdpSocket::new();
        socket
            .bind(SocketAddrEx::Ip(
                SocketAddrV4::new(local_addr.into(), 0).into(),
            ))
            .unwrap();
        let port = socket.local_addr().unwrap().into_ip().unwrap().port();
        let probe = Arc::new(IngressWakeProbe {
            ingress: ingress.clone(),
            wakes: AtomicUsize::new(0),
            has_locked_wake: AtomicBool::new(false),
        });
        let waker = Waker::from(probe.clone());
        let context = Context::from_waker(&waker);
        let mut registrations = PollRegistrations::new();
        socket
            .register(&mut registrations.context(&context), IoEvents::IN)
            .unwrap();

        let mut rx_batch = IngressBatch::with_capacity(2);
        rx_batch
            .packets
            .push(input_packet(udp_packet(port, b"first")));
        rx_batch
            .packets
            .push(input_packet(udp_packet(port, b"second")));
        let mut accepted = Vec::new();
        let mut control = Vec::new();
        assert!(try_process_rx_batch(
            &ingress,
            monotonic_time(),
            &mut rx_batch,
            &mut accepted,
            &mut control,
        ));
        assert!(rx_batch.packets.is_empty());
        assert!(accepted.is_empty());
        assert!(control.is_empty());
        assert!(probe.wakes.load(Ordering::Relaxed) > 0);
        assert!(!probe.has_locked_wake.load(Ordering::Relaxed));
        for expected in [b"first".as_slice(), b"second".as_slice()] {
            let mut bytes = [0; 32];
            let (len, _) = socket.recv_datagram_now(&mut bytes).unwrap().unwrap();
            assert_eq!(&bytes[..len], expected);
        }
    }

    #[def_test]
    fn received_fragment_publishes_expiration_deadline() {
        let ingress = Ipv4Ingress::new();
        ingress.set_ipv4_addrs(&[Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 2), 24).unwrap()]);
        let mut fragments = ipv4::fragment_output_packet(&udp_packet(9, b"fragment"), 28, 1)
            .unwrap()
            .into_iter();
        let mut rx_batch = IngressBatch::with_capacity(1);
        rx_batch
            .packets
            .push(input_packet(fragments.next().unwrap()));
        let mut accepted_packets = Vec::new();
        let mut control_packets = Vec::new();
        let timestamp = monotonic_time();

        let was_processed = ingress.with_state_locked_for_test(|| {
            try_process_rx_batch(
                &ingress,
                timestamp,
                &mut rx_batch,
                &mut accepted_packets,
                &mut control_packets,
            )
        });
        assert!(!was_processed);
        assert_eq!(rx_batch.packets.len(), 1);
        assert!(accepted_packets.is_empty());
        assert!(control_packets.is_empty());

        assert!(try_process_rx_batch(
            &ingress,
            timestamp,
            &mut rx_batch,
            &mut accepted_packets,
            &mut control_packets,
        ));
        assert!(rx_batch.packets.is_empty());
        assert!(accepted_packets.is_empty());
        assert!(control_packets.is_empty());
        let after_expiration = timestamp + TimeSpan::from_secs(60);
        assert!(ingress.handle_timer_tick(
            u64::try_from(after_expiration.span_since_origin().as_micros()).unwrap()
        ));
        let progress = expire_fragments(&ingress, after_expiration, 1, &mut control_packets);
        assert_eq!(progress.timer_events, 1);
        assert!(!progress.has_more);
        assert_eq!(control_packets.len(), 1);
        assert!(!ingress.has_pending_timer());
    }
}
