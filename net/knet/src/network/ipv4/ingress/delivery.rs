// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Transport delivery and ICMP response queuing after releasing the input lock.

use alloc::vec::Vec;

use super::{
    admission::AdmittedIpv4Packet,
    batch::{AdmittedIpPacket, IngressBatch},
};
use crate::{
    buf::PacketBuf,
    network::ipv4::{self, Icmpv4Error, Ipv4Header},
    udp, udp_err,
};

/// Delivers admitted packets in order, with no input-state lock held.
pub(super) fn deliver_batch(
    batch: &mut IngressBatch,
    accepted_packets: &mut Vec<PacketBuf>,
    control_packets: &mut Vec<Vec<u8>>,
) {
    for packet in batch.drain_admitted() {
        match packet {
            AdmittedIpPacket::Ipv4(packet) => {
                deliver_ipv4_packet(packet, accepted_packets, control_packets);
            }
            AdmittedIpPacket::Ipv6(packet) => accepted_packets.push(packet),
        }
    }
}

// Socket locks, wakeups and ICMP allocation must stay outside the input lock.
fn deliver_ipv4_packet(
    packet: AdmittedIpv4Packet,
    accepted_packets: &mut Vec<PacketBuf>,
    control_packets: &mut Vec<Vec<u8>>,
) {
    let (packet, header) = packet.into_parts();
    if header.protocol() == ipv4::PROTOCOL_UDP {
        deliver_udp_packet(packet, header, control_packets);
        return;
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

pub(super) fn queue_icmpv4_error(
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
