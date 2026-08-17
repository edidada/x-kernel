// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv6 input validation, NDP/ICMPv6, and UDP error handling.

use alloc::vec::Vec;

use super::{
    icmpv6, ndp,
    output::Ipv6Action,
    packet::{Ipv6Error, Ipv6Header, PROTOCOL_ICMPV6, PROTOCOL_TCP, PROTOCOL_UDP},
};
use crate::{
    buf::PacketBuf,
    device::NeighborLearn,
    ip::{Ipv6Address, Ipv6Cidr},
    udp, udp_err,
};

pub(crate) fn is_local_ipv6_destination(dst_addr: Ipv6Address, local_addrs: &[Ipv6Cidr]) -> bool {
    dst_addr.is_multicast()
        || dst_addr.is_loopback()
        || local_addrs.iter().any(|addr| addr.address() == dst_addr)
}

pub(crate) fn process_ipv6_input(
    mut packet: PacketBuf,
    local_addrs: &[Ipv6Cidr],
    control_packets: &mut Vec<Vec<u8>>,
    ipv6_actions: &mut Vec<Ipv6Action>,
) -> Option<PacketBuf> {
    let header = match Ipv6Header::validate_input_packet(&mut packet) {
        Ok(header) => header,
        Err(Ipv6Error::Fragmented) => return Some(packet),
        Err(Ipv6Error::Malformed) => return None,
    };

    if !is_local_ipv6_destination(header.dst_addr(), local_addrs) {
        return None;
    }

    if header.next_header() == PROTOCOL_ICMPV6 {
        let ifindex = packet.ifindex();
        let ip_packet = packet.network_packet()?;

        if let Some(input) = ndp::handle_ndp_input(header, ip_packet, local_addrs) {
            if let Some((addr, mac)) = input.learn {
                ipv6_actions.push(Ipv6Action::LearnNeighbor {
                    ifindex,
                    addr,
                    learn: NeighborLearn::unconfirmed_override(mac),
                });
            }
            if let Some(action) = input.action {
                match action {
                    ndp::NdpAction::LearnNeighbor {
                        addr,
                        mac,
                        is_solicited,
                        is_override,
                    } => {
                        ipv6_actions.push(Ipv6Action::LearnNeighbor {
                            ifindex,
                            addr,
                            learn: NeighborLearn::from_na_flags(mac, is_solicited, is_override),
                        });
                    }
                    ndp::NdpAction::ReplyAdvertisement {
                        target,
                        dst,
                        solicited,
                    } => {
                        ipv6_actions.push(Ipv6Action::ReplyAdvertisement {
                            ifindex,
                            target,
                            dst,
                            solicited,
                        });
                    }
                }
            }
            return None;
        }

        if icmpv6::is_echo_request(header, ip_packet) {
            ipv6_actions.push(Ipv6Action::EchoReply {
                ifindex,
                header,
                packet: ip_packet.to_vec(),
            });
            return None;
        }

        // NDP and echo are consumed above. Remaining ICMPv6 messages are
        // errors; deliver them to the matching UDP socket before dropping
        // the packet so they never reach smoltcp. Linux does the same split
        // in `icmpv6_rcv` (`ndisc_rcv` vs `icmpv6_notify`).
        udp_err::inspect_icmpv6_error(ip_packet, ifindex);
        return None;
    }

    if header.next_header() == PROTOCOL_UDP {
        let packet_type = packet.packet_type();
        let (disposition, returned_packet) = udp::handle_ipv6_packet(header, packet);
        match disposition {
            udp::InputDisposition::Accepted | udp::InputDisposition::Malformed => return None,
            udp::InputDisposition::NoSocket if !header.is_multicast() => {
                let packet = returned_packet?;
                let ip_packet = packet.network_packet()?;
                queue_icmpv6_error(
                    control_packets,
                    icmpv6::Icmpv6Error::PortUnreachable,
                    packet_type,
                    header,
                    ip_packet,
                );
                return None;
            }
            udp::InputDisposition::NoSocket => return None,
        }
    }

    if should_emit_parameter_problem(header) {
        if let Some(ip_packet) = packet.network_packet() {
            // RFC 4443 §3.4 Code 1: unrecognized Next Header type. The pointer
            // locates the offending next-header field (base or extension).
            queue_icmpv6_error(
                control_packets,
                icmpv6::Icmpv6Error::ParameterProblem(header.next_header_offset() as u32),
                packet.packet_type(),
                header,
                ip_packet,
            );
        }
        return None;
    }

    Some(packet)
}

fn should_emit_parameter_problem(header: Ipv6Header) -> bool {
    !matches!(
        header.next_header(),
        PROTOCOL_TCP | PROTOCOL_UDP | PROTOCOL_ICMPV6
    )
}

fn queue_icmpv6_error(
    control_packets: &mut Vec<Vec<u8>>,
    error: icmpv6::Icmpv6Error,
    packet_type: crate::buf::PacketType,
    offending_header: Ipv6Header,
    offending_packet: &[u8],
) {
    let Some(packet) =
        icmpv6::build_icmpv6_error_packet(error, packet_type, offending_header, offending_packet)
    else {
        return;
    };
    control_packets.push(packet);
}
