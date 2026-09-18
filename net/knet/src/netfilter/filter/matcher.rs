// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 `filter` table match conditions and their packet-matching behavior.
//!
//! Applies validated rule conditions to a parsed IPv4 packet using the
//! semantics of Linux 6.8 `ip_packet_match` and the built-in `tcp` / `udp` /
//! `icmp` matches.

use alloc::string::String;

use etherparse::{Icmpv4Header, TcpHeader, UdpHeaderSlice};

use crate::{
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
    stack::ipv4::{Ipv4Error, Ipv4Header, PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP},
};

/// Linux treats `frag_off == 1` (8-byte payload offset) as an evil TCP fragment.
const TCP_EVIL_FRAGMENT_OFFSET_BYTES: usize = 8;

/// A closed inclusive transport-port range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PortRange {
    start: u16,
    end: u16,
}

impl PortRange {
    pub(crate) const fn new(start: u16, end: u16) -> Option<Self> {
        if start <= end {
            Some(Self { start, end })
        } else {
            None
        }
    }

    pub(crate) const fn start(self) -> u16 {
        self.start
    }

    pub(crate) const fn end(self) -> u16 {
        self.end
    }
}

/// Interface-name condition accepted from legacy iptables syntax.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InterfaceMatch {
    Exact(String),
    Prefix(String),
}

impl InterfaceMatch {
    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Exact(name) | Self::Prefix(name) => name,
        }
    }

    pub(crate) const fn is_prefix(&self) -> bool {
        matches!(self, Self::Prefix(_))
    }
}

/// TCP flag mask and expected value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TcpFlagsMatch {
    mask: u8,
    expected: u8,
}

impl TcpFlagsMatch {
    pub(crate) const fn new(mask: u8, expected: u8) -> Option<Self> {
        if expected & !mask == 0 {
            Some(Self { mask, expected })
        } else {
            None
        }
    }

    pub(crate) const fn mask(self) -> u8 {
        self.mask
    }

    pub(crate) const fn expected(self) -> u8 {
        self.expected
    }
}

/// TCP-specific match fields. Only populated fields participate in the condition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TcpMatch {
    source_port: Option<PortRange>,
    destination_port: Option<PortRange>,
    flags: Option<TcpFlagsMatch>,
}

impl TcpMatch {
    pub(crate) const fn source_port(range: PortRange) -> Self {
        Self {
            source_port: Some(range),
            destination_port: None,
            flags: None,
        }
    }

    pub(crate) const fn destination_port(range: PortRange) -> Self {
        Self {
            source_port: None,
            destination_port: Some(range),
            flags: None,
        }
    }

    pub(crate) const fn flags(spec: TcpFlagsMatch) -> Self {
        Self {
            source_port: None,
            destination_port: None,
            flags: Some(spec),
        }
    }

    pub(crate) const fn source_port_range(self) -> Option<PortRange> {
        self.source_port
    }

    pub(crate) const fn destination_port_range(self) -> Option<PortRange> {
        self.destination_port
    }

    pub(crate) const fn flags_match(self) -> Option<TcpFlagsMatch> {
        self.flags
    }
}

/// UDP-specific match fields. Only populated fields participate in the condition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UdpMatch {
    source_port: Option<PortRange>,
    destination_port: Option<PortRange>,
}

impl UdpMatch {
    pub(crate) const fn source_port(range: PortRange) -> Self {
        Self {
            source_port: Some(range),
            destination_port: None,
        }
    }

    pub(crate) const fn destination_port(range: PortRange) -> Self {
        Self {
            source_port: None,
            destination_port: Some(range),
        }
    }

    pub(crate) const fn source_port_range(self) -> Option<PortRange> {
        self.source_port
    }

    pub(crate) const fn destination_port_range(self) -> Option<PortRange> {
        self.destination_port
    }
}

/// ICMP type and inclusive code range selected by the initial L4 matcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IcmpMatch {
    icmp_type: Option<u8>,
    code_start: u8,
    code_end: u8,
}

impl IcmpMatch {
    pub(crate) const fn new(icmp_type: Option<u8>, code_start: u8, code_end: u8) -> Option<Self> {
        if code_start <= code_end {
            Some(Self {
                icmp_type,
                code_start,
                code_end,
            })
        } else {
            None
        }
    }

    pub(crate) const fn icmp_type(self) -> Option<u8> {
        self.icmp_type
    }

    pub(crate) const fn code_start(self) -> u8 {
        self.code_start
    }

    pub(crate) const fn code_end(self) -> u8 {
        self.code_end
    }
}

/// Semantic match operations supported by the initial IPv4 filter matcher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MatchKind {
    SourceAddress(Ipv4Cidr),
    DestinationAddress(Ipv4Cidr),
    Protocol(u8),
    InputInterface(InterfaceMatch),
    OutputInterface(InterfaceMatch),
    Tcp(TcpMatch),
    Udp(UdpMatch),
    Icmp(IcmpMatch),
}

/// One independently invertible rule condition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Match {
    kind: MatchKind,
    is_inverted: bool,
}

impl Match {
    pub(crate) fn new(kind: MatchKind, is_inverted: bool) -> Self {
        Self { kind, is_inverted }
    }

    pub(crate) const fn kind(&self) -> &MatchKind {
        &self.kind
    }

    pub(crate) const fn is_inverted(&self) -> bool {
        self.is_inverted
    }
}

/// Parsed IPv4 packet metadata supplied to the evaluator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FilterPacket<'a> {
    packet: &'a [u8],
    header: Ipv4Header,
    indev: &'a str,
    outdev: &'a str,
}

impl<'a> FilterPacket<'a> {
    pub(crate) fn parse(
        packet: &'a [u8],
        indev: &'a str,
        outdev: &'a str,
    ) -> Result<Self, Ipv4Error> {
        let header = Ipv4Header::parse_input(packet)?;
        Ok(Self {
            packet,
            header,
            indev,
            outdev,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MatchOutcome {
    Matched,
    NotMatched,
    HotDrop,
}

enum L4Outcome<T> {
    Ready(T),
    Unavailable,
    HotDrop,
}

pub(crate) fn evaluate_matches(matches: &[Match], packet: &FilterPacket<'_>) -> MatchOutcome {
    for condition in matches {
        match evaluate_match(condition, packet) {
            MatchOutcome::HotDrop => return MatchOutcome::HotDrop,
            MatchOutcome::NotMatched => return MatchOutcome::NotMatched,
            MatchOutcome::Matched => {}
        }
    }
    MatchOutcome::Matched
}

fn evaluate_match(condition: &Match, packet: &FilterPacket<'_>) -> MatchOutcome {
    let matched = match condition.kind() {
        MatchKind::SourceAddress(cidr) => {
            address_matches(cidr.address(), cidr.prefix_len(), packet.header.src_addr())
        }
        MatchKind::DestinationAddress(cidr) => {
            address_matches(cidr.address(), cidr.prefix_len(), packet.header.dst_addr())
        }
        MatchKind::Protocol(protocol) => packet.header.protocol() == *protocol,
        MatchKind::InputInterface(pattern) => interface_matches(pattern, packet.indev),
        MatchKind::OutputInterface(pattern) => interface_matches(pattern, packet.outdev),
        MatchKind::Tcp(tcp) => match match_tcp(tcp, packet) {
            L4Outcome::Ready(matched) => matched,
            L4Outcome::Unavailable => return MatchOutcome::NotMatched,
            L4Outcome::HotDrop => return MatchOutcome::HotDrop,
        },
        MatchKind::Udp(udp) => match match_udp(udp, packet) {
            L4Outcome::Ready(matched) => matched,
            L4Outcome::Unavailable => return MatchOutcome::NotMatched,
            L4Outcome::HotDrop => return MatchOutcome::HotDrop,
        },
        MatchKind::Icmp(icmp) => match match_icmp(icmp, packet) {
            L4Outcome::Ready(matched) => matched,
            L4Outcome::Unavailable => return MatchOutcome::NotMatched,
            L4Outcome::HotDrop => return MatchOutcome::HotDrop,
        },
    };

    if matched ^ condition.is_inverted() {
        MatchOutcome::Matched
    } else {
        MatchOutcome::NotMatched
    }
}

fn address_matches(network: Ipv4Address, prefix_len: u8, addr: Ipv4Address) -> bool {
    crate::ip::Ipv4Cidr::new(network, prefix_len).contains_addr(&IpAddress::Ipv4(addr))
}

fn interface_matches(pattern: &InterfaceMatch, device_name: &str) -> bool {
    if pattern.is_prefix() {
        device_name.starts_with(pattern.name())
    } else {
        device_name == pattern.name()
    }
}

fn port_in_range(range: PortRange, port: u16) -> bool {
    port >= range.start() && port <= range.end()
}

fn tcp_flags_match(expected: TcpFlagsMatch, flags_byte: u8) -> bool {
    (flags_byte & expected.mask()) == expected.expected()
}

fn icmp_matches(spec: IcmpMatch, icmp_type: u8, code: u8) -> bool {
    let type_ok = spec
        .icmp_type()
        .is_none_or(|expected| expected == icmp_type);
    type_ok && code >= spec.code_start() && code <= spec.code_end()
}

fn l4_unavailable(header: Ipv4Header, l4_protocol: u8) -> bool {
    header.protocol() != l4_protocol || header.fragment_offset() != 0
}

fn transport_payload<'a>(packet: &FilterPacket<'a>) -> Option<&'a [u8]> {
    // Link-layer padding cannot supply bytes missing from the IPv4 payload.
    crate::stack::ipv4::payload(packet.packet, &packet.header)
}

fn match_tcp(spec: &TcpMatch, packet: &FilterPacket<'_>) -> L4Outcome<bool> {
    if l4_unavailable(packet.header, PROTOCOL_TCP) {
        if packet.header.protocol() == PROTOCOL_TCP
            && packet.header.fragment_offset() == TCP_EVIL_FRAGMENT_OFFSET_BYTES
        {
            return L4Outcome::HotDrop;
        }
        return L4Outcome::Unavailable;
    }

    let payload = match transport_payload(packet) {
        Some(payload) => payload,
        None => return L4Outcome::HotDrop,
    };
    // Linux net/netfilter/xt_tcpudp.c::tcp_mt only requires the fixed header
    // for port and flag matches; data offset validation belongs to option matching.
    let Some(header) = payload.get(..TcpHeader::MIN_LEN) else {
        return L4Outcome::HotDrop;
    };

    let mut matched = true;
    if let Some(range) = spec.source_port_range() {
        matched &= port_in_range(range, u16::from_be_bytes([header[0], header[1]]));
    }
    if let Some(range) = spec.destination_port_range() {
        matched &= port_in_range(range, u16::from_be_bytes([header[2], header[3]]));
    }
    if let Some(flags) = spec.flags_match() {
        matched &= tcp_flags_match(flags, header[13]);
    }

    L4Outcome::Ready(matched)
}

fn match_udp(spec: &UdpMatch, packet: &FilterPacket<'_>) -> L4Outcome<bool> {
    if l4_unavailable(packet.header, PROTOCOL_UDP) {
        return L4Outcome::Unavailable;
    }

    let payload = match transport_payload(packet) {
        Some(payload) => payload,
        None => return L4Outcome::HotDrop,
    };
    let header = match UdpHeaderSlice::from_slice(payload) {
        Ok(header) => header,
        Err(_) => return L4Outcome::HotDrop,
    };

    let mut matched = true;
    if let Some(range) = spec.source_port_range() {
        matched &= port_in_range(range, header.source_port());
    }
    if let Some(range) = spec.destination_port_range() {
        matched &= port_in_range(range, header.destination_port());
    }

    L4Outcome::Ready(matched)
}

fn match_icmp(spec: &IcmpMatch, packet: &FilterPacket<'_>) -> L4Outcome<bool> {
    if l4_unavailable(packet.header, PROTOCOL_ICMP) {
        return L4Outcome::Unavailable;
    }

    let payload = match transport_payload(packet) {
        Some(payload) => payload,
        None => return L4Outcome::HotDrop,
    };
    // Linux net/netfilter/xt_tcpudp.c::icmp_match only reads the base header;
    // timestamp body lengths do not affect type and code matching.
    let Some(header) = payload.get(..Icmpv4Header::MIN_LEN) else {
        return L4Outcome::HotDrop;
    };

    L4Outcome::Ready(icmp_matches(*spec, header[0], header[1]))
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn match_value_constructors_validate_ranges_and_masks() {
        let port_range = PortRange::new(80, 443).unwrap();
        assert_eq!(port_range.start(), 80);
        assert_eq!(port_range.end(), 443);
        assert_eq!(PortRange::new(443, 80), None);

        let flags = TcpFlagsMatch::new(0x12, 0x02).unwrap();
        assert_eq!(flags.mask(), 0x12);
        assert_eq!(flags.expected(), 0x02);
        assert_eq!(TcpFlagsMatch::new(0x02, 0x12), None);

        let icmp = IcmpMatch::new(Some(3), 0, 15).unwrap();
        assert_eq!(icmp.icmp_type(), Some(3));
        assert_eq!(icmp.code_start(), 0);
        assert_eq!(icmp.code_end(), 15);
        assert_eq!(IcmpMatch::new(Some(3), 15, 0), None);

        let tcp = TcpMatch::destination_port(port_range);
        assert_eq!(tcp.destination_port_range(), Some(port_range));
        assert_eq!(tcp.source_port_range(), None);

        let udp = UdpMatch::source_port(port_range);
        assert_eq!(udp.source_port_range(), Some(port_range));
        assert_eq!(udp.destination_port_range(), None);
    }
}
