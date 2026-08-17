// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv6 input validation, NDP/ICMPv6, and UDP error handling.

use alloc::vec::Vec;

use etherparse::icmpv6::ParameterProblemCode;
use ktime_types::MonotonicInstant;

use super::{
    fragment, icmpv6, ndp,
    output::Ipv6Action,
    packet::{
        IPV6_EXT_FRAGMENT, IPV6_MAX_PAYLOAD_LEN, Ipv6Error, Ipv6Fragment, Ipv6Header,
        PROTOCOL_ICMPV6, PROTOCOL_TCP, PROTOCOL_UDP, first_fragment_upper_header_is_truncated,
        strip_ipv6_fragment_header,
    },
};
use crate::{
    buf::{PacketBuf, PacketOwner, PacketType},
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
    timestamp: MonotonicInstant,
    mut packet: PacketBuf,
    local_addrs: &[Ipv6Cidr],
    reassembler: &mut fragment::Ipv6Reassembler,
    control_packets: &mut Vec<Vec<u8>>,
    ipv6_actions: &mut Vec<Ipv6Action>,
) -> Option<PacketBuf> {
    let header = match Ipv6Header::validate_input_packet_allowing_fragment(&mut packet) {
        Ok(header) => header,
        Err(Ipv6Error::Malformed | Ipv6Error::Fragmented) => return None,
    };

    if !is_local_ipv6_destination(header.dst_addr(), local_addrs) {
        return None;
    }

    if header.next_header() == IPV6_EXT_FRAGMENT {
        return process_ipv6_fragment(
            timestamp,
            packet,
            header,
            local_addrs,
            reassembler,
            control_packets,
            ipv6_actions,
        );
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
                icmpv6::Icmpv6Error::ParameterProblem {
                    code: ParameterProblemCode::UnrecognizedNextHeader,
                    pointer: header.next_header_offset() as u32,
                },
                packet.packet_type(),
                header,
                ip_packet,
            );
        }
        return None;
    }

    Some(packet)
}

fn process_ipv6_fragment(
    timestamp: MonotonicInstant,
    packet: PacketBuf,
    header: Ipv6Header,
    local_addrs: &[Ipv6Cidr],
    reassembler: &mut fragment::Ipv6Reassembler,
    control_packets: &mut Vec<Vec<u8>>,
    ipv6_actions: &mut Vec<Ipv6Action>,
) -> Option<PacketBuf> {
    let packet_type = packet.packet_type();
    let ifindex = packet.ifindex();
    let fragment = match packet
        .network_packet()
        .and_then(|ip_packet| Ipv6Fragment::parse(ip_packet, &header).ok())
    {
        Some(fragment) => fragment,
        None => {
            if let Some(ip_packet) = packet.network_packet() {
                queue_icmpv6_error(
                    control_packets,
                    icmpv6::Icmpv6Error::ParameterProblem {
                        code: ParameterProblemCode::ErroneousHeaderField,
                        pointer: header.header_len() as u32,
                    },
                    packet_type,
                    header,
                    ip_packet,
                );
            }
            return None;
        }
    };

    // A nested Fragment next-header would re-enter this path after strip or
    // reassembly. Linux rejects that with `IP6SKB_FRAGMENTED` already set.
    if fragment.next_header == IPV6_EXT_FRAGMENT {
        return None;
    }

    if fragment.is_atomic() {
        let stripped =
            strip_ipv6_fragment_header(packet.network_packet()?, &header, fragment).ok()?;
        let packet = PacketBuf::from_ip_packet_vec_with_type(
            ifindex,
            stripped,
            PacketOwner::Ipv6Stack,
            packet_type,
        );
        return process_ipv6_input(
            timestamp,
            packet,
            local_addrs,
            reassembler,
            control_packets,
            ipv6_actions,
        );
    }

    if let Some(error) = fragment_parameter_problem(&packet, header, fragment) {
        if let Some(ip_packet) = packet.network_packet() {
            queue_icmpv6_error(control_packets, error, packet_type, header, ip_packet);
        }
        return None;
    }

    match reassembler.reassemble(packet, header, fragment, timestamp) {
        fragment::Ipv6ReassemblyResult::Complete(packet) => process_ipv6_input(
            timestamp,
            packet,
            local_addrs,
            reassembler,
            control_packets,
            ipv6_actions,
        ),
        fragment::Ipv6ReassemblyResult::Pending | fragment::Ipv6ReassemblyResult::Dropped => None,
    }
}

fn fragment_parameter_problem(
    packet: &PacketBuf,
    header: Ipv6Header,
    fragment: Ipv6Fragment,
) -> Option<icmpv6::Icmpv6Error> {
    let ip_packet = packet.network_packet()?;
    let fragmentable = fragment.payload(ip_packet, &header)?;
    let end = fragment.offset.checked_add(fragmentable.len())?;
    // Linux `ipv6_frag_rcv` checks a truncated first-fragment header chain
    // before `ip6_frag_queue` looks at 8-byte alignment.
    if fragment.offset == 0
        && first_fragment_upper_header_is_truncated(fragment.next_header, fragmentable)
    {
        return Some(icmpv6::Icmpv6Error::ParameterProblem {
            code: ParameterProblemCode::Ipv6FirstFragmentIncompleteHeaderChain,
            pointer: 0,
        });
    }
    if end > IPV6_MAX_PAYLOAD_LEN {
        return Some(icmpv6::Icmpv6Error::ParameterProblem {
            code: ParameterProblemCode::ErroneousHeaderField,
            pointer: Ipv6Fragment::frag_off_pointer(&header),
        });
    }
    // RFC 8200 §4.5: non-final fragments must be a multiple of 8 octets.
    // Linux `ip6_frag_queue` points the Parameter Problem at payload_len.
    if fragment.more_fragments && end & 0x7 != 0 {
        return Some(icmpv6::Icmpv6Error::ParameterProblem {
            code: ParameterProblemCode::ErroneousHeaderField,
            pointer: 4,
        });
    }
    None
}

fn should_emit_parameter_problem(header: Ipv6Header) -> bool {
    !matches!(
        header.next_header(),
        PROTOCOL_TCP | PROTOCOL_UDP | PROTOCOL_ICMPV6
    )
}

pub(crate) fn queue_icmpv6_error(
    control_packets: &mut Vec<Vec<u8>>,
    error: icmpv6::Icmpv6Error,
    packet_type: PacketType,
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
