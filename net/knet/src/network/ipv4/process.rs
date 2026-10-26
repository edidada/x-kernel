// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 payload access and ICMP error construction.

use alloc::{vec, vec::Vec};

use etherparse::{
    Icmpv4Header, Icmpv4Type, Ipv4Header, Ipv4HeaderSlice,
    icmpv4::{DestUnreachableHeader, TimeExceededCode},
};

use super::write_ipv4_packet_header;
use crate::{buf::PacketType, ip::Ipv4Address};

pub(crate) const PROTOCOL_ICMP: u8 = 1;
pub(crate) const PROTOCOL_TCP: u8 = 6;
pub(crate) const PROTOCOL_UDP: u8 = 17;

pub(crate) const IPV4_MIN_HEADER_LEN: usize = 20;
const IPV4_DEFAULT_TTL: u8 = 64;
const ICMPV4_ERROR_QUOTE_BYTES: usize = 8;
// ICMPv4 types through Address Mask Reply have explicit request, reply, or
// error policy below. Higher values are conservatively treated as errors.
const ICMPV4_EXPLICIT_POLICY_MAX_TYPE: u8 = 18;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Icmpv4Error {
    ProtocolUnreachable,
    PortUnreachable,
    FragmentReassemblyTimeout,
}

pub(crate) fn ip_version(packet: &[u8]) -> Option<u8> {
    packet.first().map(|byte| byte >> 4)
}

pub(crate) fn payload<'a>(packet: &'a [u8], header: &Ipv4HeaderSlice<'_>) -> Option<&'a [u8]> {
    packet.get(header.slice().len()..usize::from(header.total_len()))
}

pub(crate) fn build_icmpv4_error_packet(
    error: Icmpv4Error,
    packet_type: PacketType,
    offending_header: &Ipv4Header,
    offending_packet: &[u8],
) -> Option<Vec<u8>> {
    if !can_send_icmpv4_error(packet_type, offending_header, offending_packet) {
        return None;
    }

    let quote_len = usize::from(offending_header.total_len)
        .min(offending_header.header_len() + ICMPV4_ERROR_QUOTE_BYTES);
    let icmp_len = Icmpv4Header::MIN_LEN + quote_len;
    let total_len = IPV4_MIN_HEADER_LEN + icmp_len;
    let mut packet = vec![0u8; total_len];

    write_ipv4_packet_header(
        &mut packet,
        Ipv4Address::from_octets(offending_header.destination),
        Ipv4Address::from_octets(offending_header.source),
        PROTOCOL_ICMP,
        IPV4_DEFAULT_TTL,
        false,
    )?;

    let quoted_packet = offending_packet.get(..quote_len)?;
    let icmp_type = match error {
        Icmpv4Error::ProtocolUnreachable => {
            Icmpv4Type::DestinationUnreachable(DestUnreachableHeader::Protocol)
        }
        Icmpv4Error::PortUnreachable => {
            Icmpv4Type::DestinationUnreachable(DestUnreachableHeader::Port)
        }
        Icmpv4Error::FragmentReassemblyTimeout => {
            Icmpv4Type::TimeExceeded(TimeExceededCode::FragmentReassemblyTimeExceeded)
        }
    };
    let icmp_header = Icmpv4Header::with_checksum(icmp_type, quoted_packet);
    let icmp = &mut packet[IPV4_MIN_HEADER_LEN..];
    icmp[..Icmpv4Header::MIN_LEN].copy_from_slice(&icmp_header.to_bytes());
    icmp[Icmpv4Header::MIN_LEN..].copy_from_slice(quoted_packet);

    Some(packet)
}

#[cfg(unittest)]
pub(crate) fn build_ipv4_packet(
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    protocol: u8,
    ttl: u8,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let total_len = IPV4_MIN_HEADER_LEN.checked_add(payload.len())?;
    let mut packet = vec![0u8; total_len];
    write_ipv4_packet_header(&mut packet, src_addr, dst_addr, protocol, ttl, false)?;
    packet[IPV4_MIN_HEADER_LEN..].copy_from_slice(payload);
    Some(packet)
}

#[cfg(unittest)]
pub(crate) fn build_ipv4_packet_dont_fragment(
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    protocol: u8,
    ttl: u8,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let total_len = IPV4_MIN_HEADER_LEN.checked_add(payload.len())?;
    let mut packet = vec![0u8; total_len];
    write_ipv4_packet_header(&mut packet, src_addr, dst_addr, protocol, ttl, true)?;
    packet[IPV4_MIN_HEADER_LEN..].copy_from_slice(payload);
    Some(packet)
}

fn can_send_icmpv4_error(
    packet_type: PacketType,
    offending_header: &Ipv4Header,
    offending_packet: &[u8],
) -> bool {
    if packet_type != PacketType::Host {
        return false;
    }

    if Ipv4Address::from_octets(offending_header.source).is_unspecified()
        || Ipv4Address::from_octets(offending_header.destination).is_broadcast()
        || Ipv4Address::from_octets(offending_header.destination).is_multicast()
    {
        return false;
    }

    if offending_header.fragment_offset.value() != 0 {
        return false;
    }

    if offending_header.protocol.0 != PROTOCOL_ICMP {
        return true;
    }

    let Some(icmp_type) = offending_packet
        .get(offending_header.header_len()..usize::from(offending_header.total_len))
        .and_then(|payload| payload.first().copied())
    else {
        return false;
    };

    !is_icmpv4_error_type(icmp_type)
}

fn is_icmpv4_error_type(icmp_type: u8) -> bool {
    // Unknown or discarded ICMP control types are treated as errors so that
    // error generation cannot recurse on packets the stack will not handle.
    matches!(icmp_type, 1..=7 | 9..=12) || icmp_type > ICMPV4_EXPLICIT_POLICY_MAX_TYPE
}

unittest::path_mod_test!(tests, "../../../ut-tests/network/ipv4/process/tests.rs");
