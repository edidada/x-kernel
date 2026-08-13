// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 input validation and UDP/ICMPv4 error handling.

use alloc::vec::Vec;

use ktime_types::MonotonicInstant;

use super::{
    fragment,
    packet::{
        Icmpv4Error, Ipv4Error, Ipv4Header, PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP,
        build_icmpv4_error_packet,
    },
};
use crate::{
    buf::{PacketBuf, PacketType},
    ip::{Ipv4Address, Ipv4Cidr},
    udp,
};

pub(crate) fn is_local_ipv4_destination(dst_addr: Ipv4Address, local_addrs: &[Ipv4Cidr]) -> bool {
    dst_addr.is_broadcast()
        || dst_addr.is_multicast()
        || local_addrs
            .iter()
            .any(|addr| addr.address() == dst_addr || addr.broadcast() == Some(dst_addr))
}

pub(crate) fn process_ipv4_input(
    timestamp: MonotonicInstant,
    mut packet: PacketBuf,
    local_addrs: &[Ipv4Cidr],
    reassembler: &mut fragment::Ipv4Reassembler,
    control_packets: &mut Vec<Vec<u8>>,
) -> Option<PacketBuf> {
    let header = match Ipv4Header::validate_input_packet(&mut packet) {
        Ok(header) => header,
        Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return None,
    };

    if !is_local_ipv4_destination(header.dst_addr(), local_addrs) {
        return None;
    }

    if header.is_fragmented() {
        match reassembler.reassemble(packet, header, timestamp) {
            fragment::Ipv4ReassemblyResult::Complete(packet) => {
                return process_ipv4_input(
                    timestamp,
                    packet,
                    local_addrs,
                    reassembler,
                    control_packets,
                );
            }
            fragment::Ipv4ReassemblyResult::Pending | fragment::Ipv4ReassemblyResult::Dropped => {}
        }
        return None;
    }

    if header.protocol() == PROTOCOL_UDP {
        let packet_type = packet.packet_type();
        let (disposition, returned_packet) = udp::handle_ipv4_packet(header, packet);
        match disposition {
            udp::InputDisposition::Accepted | udp::InputDisposition::Malformed => return None,
            udp::InputDisposition::NoSocket if !header.is_broadcast_or_multicast() => {
                let packet = returned_packet?;
                let ip_packet = packet.network_packet()?;
                queue_icmpv4_error(
                    control_packets,
                    Icmpv4Error::PortUnreachable,
                    packet_type,
                    header,
                    ip_packet,
                );
                return None;
            }
            udp::InputDisposition::NoSocket => return None,
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
        return None;
    }

    Some(packet)
}

pub(crate) fn queue_icmpv4_error(
    control_packets: &mut Vec<Vec<u8>>,
    error: Icmpv4Error,
    packet_type: PacketType,
    offending_header: Ipv4Header,
    offending_packet: &[u8],
) {
    let Some(packet) =
        build_icmpv4_error_packet(error, packet_type, offending_header, offending_packet)
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
            PROTOCOL_TCP | PROTOCOL_UDP | PROTOCOL_ICMP
        )
}
