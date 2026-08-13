// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ICMPv6 echo reply and error helpers.

use alloc::{vec, vec::Vec};

use etherparse::{
    Icmpv6Header, Icmpv6Slice, Icmpv6Type,
    icmpv6::{DestUnreachableCode, ParameterProblemCode, ParameterProblemHeader},
};

use super::{self as ipv6, IPV6_DEFAULT_HOP_LIMIT, IPV6_HEADER_LEN, Ipv6Header, PROTOCOL_ICMPV6};
use crate::{buf::PacketType, ip::Ipv6Address};

const ICMPV6_ERROR_QUOTE_BYTES: usize = 1232;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Icmpv6Error {
    PortUnreachable,
    /// RFC 4443 §3.4 Parameter Problem, Code 1 (unrecognized Next Header).
    ///
    /// The `pointer` is the octet offset of the offending next-header field
    /// within the offending packet.
    ParameterProblem(u32),
}

/// Builds an ICMPv6 echo reply for a validated echo request packet.
///
/// `reply_src` is the source address of the reply. For unicast requests it
/// must be the request destination; for multicast/anycast requests RFC 4443
/// §4.2 requires a unicast address of the receiving interface.
pub(crate) fn build_echo_reply(
    request_header: Ipv6Header,
    request_packet: &[u8],
    reply_src: Ipv6Address,
) -> Option<Vec<u8>> {
    let icmp = ipv6::payload(request_packet, &request_header)?;
    let slice = Icmpv6Slice::from_slice(icmp).ok()?;
    let Icmpv6Type::EchoRequest(echo) = slice.icmp_type() else {
        return None;
    };
    if request_header.src_addr().is_unspecified() || request_header.src_addr().is_multicast() {
        return None;
    }
    if !has_valid_checksum(request_header.src_addr(), request_header.dst_addr(), icmp) {
        return None;
    }

    let echo_payload = slice.payload();
    let reply_type = Icmpv6Type::EchoReply(echo);
    let icmp_header = Icmpv6Header::with_checksum(
        reply_type,
        reply_src.octets(),
        request_header.src_addr().octets(),
        echo_payload,
    )
    .ok()?;

    let icmp_len = icmp_header.header_len() + echo_payload.len();
    let mut packet = vec![0u8; IPV6_HEADER_LEN + icmp_len];
    ipv6::write_ipv6_packet_header(
        &mut packet,
        reply_src,
        request_header.src_addr(),
        PROTOCOL_ICMPV6,
        IPV6_DEFAULT_HOP_LIMIT,
    )?;
    let icmp_bytes = icmp_header.to_bytes();
    packet[IPV6_HEADER_LEN..IPV6_HEADER_LEN + icmp_bytes.len()].copy_from_slice(&icmp_bytes);
    packet[IPV6_HEADER_LEN + icmp_bytes.len()..].copy_from_slice(echo_payload);
    Some(packet)
}

pub(crate) fn build_icmpv6_error_packet(
    error: Icmpv6Error,
    packet_type: PacketType,
    offending_header: Ipv6Header,
    offending_packet: &[u8],
) -> Option<Vec<u8>> {
    if !can_send_icmpv6_error(packet_type, offending_header, offending_packet) {
        return None;
    }

    let quote_len = offending_header
        .total_len()
        .min(ICMPV6_ERROR_QUOTE_BYTES)
        .min(offending_packet.len());
    let quoted = offending_packet.get(..quote_len)?;

    let icmp_type = match error {
        Icmpv6Error::PortUnreachable => {
            Icmpv6Type::DestinationUnreachable(DestUnreachableCode::Port)
        }
        Icmpv6Error::ParameterProblem(pointer) => {
            Icmpv6Type::ParameterProblem(ParameterProblemHeader {
                code: ParameterProblemCode::UnrecognizedNextHeader,
                pointer,
            })
        }
    };
    let icmp_header = Icmpv6Header::with_checksum(
        icmp_type,
        offending_header.dst_addr().octets(),
        offending_header.src_addr().octets(),
        quoted,
    )
    .ok()?;

    let icmp_len = icmp_header.header_len() + quoted.len();
    let mut packet = vec![0u8; IPV6_HEADER_LEN + icmp_len];
    ipv6::write_ipv6_packet_header(
        &mut packet,
        offending_header.dst_addr(),
        offending_header.src_addr(),
        PROTOCOL_ICMPV6,
        IPV6_DEFAULT_HOP_LIMIT,
    )?;
    let icmp_bytes = icmp_header.to_bytes();
    packet[IPV6_HEADER_LEN..IPV6_HEADER_LEN + icmp_bytes.len()].copy_from_slice(&icmp_bytes);
    packet[IPV6_HEADER_LEN + icmp_bytes.len()..].copy_from_slice(quoted);
    Some(packet)
}

pub(crate) fn is_echo_request(header: Ipv6Header, packet: &[u8]) -> bool {
    if header.next_header() != PROTOCOL_ICMPV6 {
        return false;
    }
    let Some(icmp) = ipv6::payload(packet, &header) else {
        return false;
    };
    matches!(
        Icmpv6Slice::from_slice(icmp).map(|s| s.icmp_type()),
        Ok(Icmpv6Type::EchoRequest(_))
    )
}

pub(crate) fn has_valid_checksum(
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
    icmp_packet: &[u8],
) -> bool {
    let Ok(slice) = Icmpv6Slice::from_slice(icmp_packet) else {
        return false;
    };
    let Ok(expected) =
        slice
            .icmp_type()
            .calc_checksum(src_addr.octets(), dst_addr.octets(), slice.payload())
    else {
        return false;
    };
    expected == slice.checksum()
}

fn can_send_icmpv6_error(
    packet_type: PacketType,
    offending_header: Ipv6Header,
    offending_packet: &[u8],
) -> bool {
    if packet_type != PacketType::Host {
        return false;
    }
    if offending_header.src_addr().is_unspecified()
        || offending_header.src_addr().is_multicast()
        || offending_header.is_multicast()
    {
        return false;
    }
    if offending_header.next_header() != PROTOCOL_ICMPV6 {
        return true;
    }
    let Some(icmp) = ipv6::payload(offending_packet, &offending_header) else {
        return false;
    };
    let Some(icmp_type) = Icmpv6Slice::from_slice(icmp).ok().map(|s| s.type_u8()) else {
        return false;
    };
    // RFC 4443 §2.4(e): never answer an ICMPv6 error (type < 128) or a
    // Redirect (type 137) with another error.
    icmp_type >= 128 && icmp_type != 137
}

#[cfg(unittest)]
mod tests {
    use etherparse::IcmpEchoHeader;
    use unittest::def_test;

    use super::{
        super::{PROTOCOL_UDP, build_ipv6_packet},
        *,
    };

    fn sample_addrs() -> (Ipv6Address, Ipv6Address) {
        (
            Ipv6Address::from_octets([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            Ipv6Address::from_octets([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]),
        )
    }

    #[def_test]
    fn test_build_echo_reply() {
        let (src, dst) = sample_addrs();
        let data = [9, 8, 7, 6];
        let icmp = {
            let header = Icmpv6Header::with_checksum(
                Icmpv6Type::EchoRequest(IcmpEchoHeader { id: 1, seq: 2 }),
                src.octets(),
                dst.octets(),
                &data,
            )
            .unwrap();
            let mut bytes = header.to_bytes().to_vec();
            bytes.extend_from_slice(&data);
            bytes
        };
        let packet = build_ipv6_packet(src, dst, PROTOCOL_ICMPV6, 64, &icmp).unwrap();
        let header = Ipv6Header::parse_input(&packet).unwrap();

        let reply = build_echo_reply(header, &packet, dst).expect("echo reply");
        let reply_header = Ipv6Header::parse_input(&reply).unwrap();
        assert_eq!(reply_header.src_addr(), dst);
        assert_eq!(reply_header.dst_addr(), src);
        assert_eq!(reply_header.next_header(), PROTOCOL_ICMPV6);
        let reply_icmp = ipv6::payload(&reply, &reply_header).unwrap();
        let slice = Icmpv6Slice::from_slice(reply_icmp).unwrap();
        assert!(matches!(slice.icmp_type(), Icmpv6Type::EchoReply(_)));
        assert_eq!(slice.payload(), &data);
    }

    #[def_test]
    fn test_echo_reply_to_multicast_uses_unicast_source() {
        let (src, _) = sample_addrs();
        let multicast_dst =
            Ipv6Address::from_octets([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let reply_src =
            Ipv6Address::from_octets([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);
        let data = [9, 8, 7, 6];
        let icmp = {
            let header = Icmpv6Header::with_checksum(
                Icmpv6Type::EchoRequest(IcmpEchoHeader { id: 1, seq: 2 }),
                src.octets(),
                multicast_dst.octets(),
                &data,
            )
            .unwrap();
            let mut bytes = header.to_bytes().to_vec();
            bytes.extend_from_slice(&data);
            bytes
        };
        let packet = build_ipv6_packet(src, multicast_dst, PROTOCOL_ICMPV6, 64, &icmp).unwrap();
        let header = Ipv6Header::parse_input(&packet).unwrap();

        let reply = build_echo_reply(header, &packet, reply_src).expect("echo reply");
        let reply_header = Ipv6Header::parse_input(&reply).unwrap();
        assert_eq!(reply_header.src_addr(), reply_src);
        assert_eq!(reply_header.dst_addr(), src);
    }

    #[def_test]
    fn test_echo_reply_rejects_multicast_and_unspecified_sources() {
        let (_, dst) = sample_addrs();
        for bad_src in [
            Ipv6Address::UNSPECIFIED,
            Ipv6Address::from_octets([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
        ] {
            let icmp = {
                let header = Icmpv6Header::with_checksum(
                    Icmpv6Type::EchoRequest(IcmpEchoHeader { id: 1, seq: 2 }),
                    bad_src.octets(),
                    dst.octets(),
                    &[1, 2, 3, 4],
                )
                .unwrap();
                let mut bytes = header.to_bytes().to_vec();
                bytes.extend_from_slice(&[1, 2, 3, 4]);
                bytes
            };
            let packet = build_ipv6_packet(bad_src, dst, PROTOCOL_ICMPV6, 64, &icmp).unwrap();
            let header = Ipv6Header::parse_input(&packet).unwrap();
            assert!(build_echo_reply(header, &packet, dst).is_none());
        }
    }

    #[def_test]
    fn test_build_port_unreachable() {
        let (src, dst) = sample_addrs();
        let offending = build_ipv6_packet(src, dst, PROTOCOL_UDP, 64, &[0; 8]).unwrap();
        let header = Ipv6Header::parse_input(&offending).unwrap();

        let error = build_icmpv6_error_packet(
            Icmpv6Error::PortUnreachable,
            PacketType::Host,
            header,
            &offending,
        )
        .expect("ICMPv6 error");
        let error_header = Ipv6Header::parse_input(&error).unwrap();
        assert_eq!(error_header.src_addr(), dst);
        assert_eq!(error_header.dst_addr(), src);
        let icmp = ipv6::payload(&error, &error_header).unwrap();
        assert_eq!(icmp[0], etherparse::icmpv6::TYPE_DST_UNREACH);
        assert_eq!(icmp[1], etherparse::icmpv6::CODE_DST_UNREACH_PORT);
    }

    #[def_test]
    fn test_build_parameter_problem() {
        let (src, dst) = sample_addrs();
        // Protocol 132 (SCTP) is not handled by this stack.
        let offending = build_ipv6_packet(src, dst, 132, 64, &[0; 8]).unwrap();
        let header = Ipv6Header::parse_input(&offending).unwrap();
        assert_eq!(header.next_header_offset(), 6);

        let error = build_icmpv6_error_packet(
            Icmpv6Error::ParameterProblem(header.next_header_offset() as u32),
            PacketType::Host,
            header,
            &offending,
        )
        .expect("ICMPv6 error");
        let error_header = Ipv6Header::parse_input(&error).unwrap();
        assert_eq!(error_header.src_addr(), dst);
        assert_eq!(error_header.dst_addr(), src);
        let icmp = ipv6::payload(&error, &error_header).unwrap();
        assert_eq!(icmp[0], etherparse::icmpv6::TYPE_PARAMETER_PROBLEM);
        assert_eq!(
            icmp[1],
            etherparse::icmpv6::CODE_PARAM_PROBLEM_UNRECOG_NEXT_HEADER
        );
        assert_eq!(
            u32::from_be_bytes([icmp[4], icmp[5], icmp[6], icmp[7]]),
            header.next_header_offset() as u32
        );
    }
}
