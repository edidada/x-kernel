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

use super::model::Rule;
use crate::{
    ip::{IpAddress, Ipv4Address, Ipv4Cidr},
    ipv4::{Ipv4Error, Ipv4Header, PROTOCOL_ICMP, PROTOCOL_TCP, PROTOCOL_UDP},
};

/// Linux treats `frag_off == 1` (8-byte payload offset) as an evil TCP fragment.
const TCP_EVIL_FRAGMENT_OFFSET_BYTES: usize = 8;

/// A condition value with an independently controlled logical inversion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Negated<T> {
    value: T,
    is_inverted: bool,
}

impl<T> Negated<T> {
    pub(crate) const fn new(value: T, is_inverted: bool) -> Self {
        Self { value, is_inverted }
    }

    fn matches(&self, match_fn: impl FnOnce(&T) -> bool) -> bool {
        match_fn(&self.value) ^ self.is_inverted
    }

    pub(crate) const fn value(&self) -> &T {
        &self.value
    }

    pub(crate) const fn is_inverted(&self) -> bool {
        self.is_inverted
    }
}

/// Fixed IPv4 rule fields, evaluated before any extension inspects the payload.
///
/// Absent fields impose no condition; populated fields are ANDed together.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BaseMatch {
    pub(crate) source: Option<Negated<Ipv4Cidr>>,
    pub(crate) destination: Option<Negated<Ipv4Cidr>>,
    pub(crate) input_interface: Option<Negated<InterfaceMatch>>,
    pub(crate) output_interface: Option<Negated<InterfaceMatch>>,
    pub(crate) protocol: Option<Negated<u8>>,
}

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

    pub(crate) const fn contains(self, port: u16) -> bool {
        self.start <= port && port <= self.end
    }

    pub(super) const fn start(self) -> u16 {
        self.start
    }

    pub(super) const fn end(self) -> u16 {
        self.end
    }
}

/// Interface-name condition accepted from legacy iptables syntax.
///
/// Linux interface names occupy a 16-byte array including the trailing NUL.
pub(crate) const MAX_INTERFACE_NAME_LEN: usize = 15;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InterfaceMatch {
    name: String,
    is_prefix: bool,
}

impl InterfaceMatch {
    pub(crate) fn exact(name: String) -> Option<Self> {
        Self::new(name, false)
    }

    pub(crate) fn prefix(name: String) -> Option<Self> {
        Self::new(name, true)
    }

    fn new(name: String, is_prefix: bool) -> Option<Self> {
        (name.len() <= MAX_INTERFACE_NAME_LEN).then_some(Self { name, is_prefix })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn is_prefix(&self) -> bool {
        self.is_prefix
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

/// TCP conditions, ANDed after applying each populated field's inversion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TcpMatch {
    source_port: Option<Negated<PortRange>>,
    destination_port: Option<Negated<PortRange>>,
    flags: Option<Negated<TcpFlagsMatch>>,
}

impl TcpMatch {
    pub(crate) const fn new() -> Self {
        Self {
            source_port: None,
            destination_port: None,
            flags: None,
        }
    }

    pub(crate) const fn with_source_port(mut self, range: Negated<PortRange>) -> Self {
        self.source_port = Some(range);
        self
    }

    pub(crate) const fn with_destination_port(mut self, range: Negated<PortRange>) -> Self {
        self.destination_port = Some(range);
        self
    }

    pub(crate) const fn with_flags(mut self, flags: Negated<TcpFlagsMatch>) -> Self {
        self.flags = Some(flags);
        self
    }

    pub(crate) const fn source_port_range(self) -> Option<Negated<PortRange>> {
        self.source_port
    }

    pub(crate) const fn destination_port_range(self) -> Option<Negated<PortRange>> {
        self.destination_port
    }

    pub(crate) const fn flags_match(self) -> Option<Negated<TcpFlagsMatch>> {
        self.flags
    }
}

/// UDP conditions, ANDed after applying each populated field's inversion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct UdpMatch {
    source_port: Option<Negated<PortRange>>,
    destination_port: Option<Negated<PortRange>>,
}

impl UdpMatch {
    pub(crate) const fn new() -> Self {
        Self {
            source_port: None,
            destination_port: None,
        }
    }

    pub(crate) const fn with_source_port(mut self, range: Negated<PortRange>) -> Self {
        self.source_port = Some(range);
        self
    }

    pub(crate) const fn with_destination_port(mut self, range: Negated<PortRange>) -> Self {
        self.destination_port = Some(range);
        self
    }

    pub(crate) const fn source_port_range(self) -> Option<Negated<PortRange>> {
        self.source_port
    }

    pub(crate) const fn destination_port_range(self) -> Option<Negated<PortRange>> {
        self.destination_port
    }
}

/// ICMP type and inclusive code range with joint inversion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IcmpMatch {
    icmp_type: Option<u8>,
    code_start: u8,
    code_end: u8,
    is_inverted: bool,
}

impl IcmpMatch {
    pub(crate) const fn new(icmp_type: Option<u8>, code_start: u8, code_end: u8) -> Option<Self> {
        if code_start <= code_end {
            Some(Self {
                icmp_type,
                code_start,
                code_end,
                is_inverted: false,
            })
        } else {
            None
        }
    }

    pub(crate) const fn icmp_type(self) -> Option<u8> {
        self.icmp_type
    }

    /// Inverts the combined type and code condition after header validation.
    pub(crate) const fn with_inversion(mut self, is_inverted: bool) -> Self {
        self.is_inverted = is_inverted;
        self
    }

    pub(crate) const fn code_start(self) -> u8 {
        self.code_start
    }

    pub(crate) const fn code_end(self) -> u8 {
        self.code_end
    }

    pub(super) const fn is_inverted(self) -> bool {
        self.is_inverted
    }
}

/// Ordered extension matches following the fixed IPv4 rule fields.
///
/// Each extension owns its field-level inversion semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExtensionMatch {
    Tcp(TcpMatch),
    Udp(UdpMatch),
    Icmp(IcmpMatch),
}

impl ExtensionMatch {
    pub(super) const fn required_protocol(&self) -> u8 {
        match self {
            Self::Tcp(_) => PROTOCOL_TCP,
            Self::Udp(_) => PROTOCOL_UDP,
            Self::Icmp(_) => PROTOCOL_ICMP,
        }
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

/// Checks the base fields before evaluating extensions in their stored order.
pub(crate) fn evaluate_rule(rule: &Rule, packet: &FilterPacket<'_>) -> MatchOutcome {
    if !evaluate_base_match(rule.base(), packet) {
        return MatchOutcome::NotMatched;
    }

    for extension in rule.extensions() {
        match evaluate_extension_match(extension, packet) {
            MatchOutcome::Matched => {}
            outcome => return outcome,
        }
    }
    MatchOutcome::Matched
}

fn evaluate_base_match(base: &BaseMatch, packet: &FilterPacket<'_>) -> bool {
    base.source.as_ref().is_none_or(|condition| {
        condition.matches(|cidr| address_matches(*cidr, packet.header.src_addr()))
    }) && base.destination.as_ref().is_none_or(|condition| {
        condition.matches(|cidr| address_matches(*cidr, packet.header.dst_addr()))
    }) && base.input_interface.as_ref().is_none_or(|condition| {
        condition.matches(|pattern| interface_matches(pattern, packet.indev))
    }) && base.output_interface.as_ref().is_none_or(|condition| {
        condition.matches(|pattern| interface_matches(pattern, packet.outdev))
    }) && base
        .protocol
        .as_ref()
        .is_none_or(|condition| condition.matches(|protocol| packet.header.protocol() == *protocol))
}

fn evaluate_extension_match(extension: &ExtensionMatch, packet: &FilterPacket<'_>) -> MatchOutcome {
    let outcome = match extension {
        ExtensionMatch::Tcp(tcp) => match_tcp(tcp, packet),
        ExtensionMatch::Udp(udp) => match_udp(udp, packet),
        ExtensionMatch::Icmp(icmp) => match_icmp(icmp, packet),
    };
    match outcome {
        L4Outcome::Ready(true) => MatchOutcome::Matched,
        L4Outcome::Ready(false) | L4Outcome::Unavailable => MatchOutcome::NotMatched,
        L4Outcome::HotDrop => MatchOutcome::HotDrop,
    }
}

fn address_matches(cidr: Ipv4Cidr, addr: Ipv4Address) -> bool {
    cidr.contains_addr(&IpAddress::Ipv4(addr))
}

fn interface_matches(pattern: &InterfaceMatch, device_name: &str) -> bool {
    if pattern.is_prefix() {
        device_name.starts_with(pattern.name())
    } else {
        device_name == pattern.name()
    }
}

fn tcp_flags_match(expected: TcpFlagsMatch, flags_byte: u8) -> bool {
    (flags_byte & expected.mask()) == expected.expected()
}

fn icmp_matches(spec: IcmpMatch, icmp_type: u8, code: u8) -> bool {
    let type_ok = spec
        .icmp_type()
        .is_none_or(|expected| expected == icmp_type);
    (type_ok && code >= spec.code_start() && code <= spec.code_end()) ^ spec.is_inverted
}

fn l4_unavailable(header: Ipv4Header, l4_protocol: u8) -> bool {
    header.protocol() != l4_protocol || header.fragment_offset() != 0
}

fn transport_payload<'a>(packet: &FilterPacket<'a>) -> Option<&'a [u8]> {
    // Link-layer padding cannot supply bytes missing from the IPv4 payload.
    crate::ipv4::payload(packet.packet, &packet.header)
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
        matched &=
            range.matches(|range| range.contains(u16::from_be_bytes([header[0], header[1]])));
    }
    if let Some(range) = spec.destination_port_range() {
        matched &=
            range.matches(|range| range.contains(u16::from_be_bytes([header[2], header[3]])));
    }
    if let Some(flags) = spec.flags_match() {
        matched &= flags.matches(|flags| tcp_flags_match(*flags, header[13]));
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
        matched &= range.matches(|range| range.contains(header.source_port()));
    }
    if let Some(range) = spec.destination_port_range() {
        matched &= range.matches(|range| range.contains(header.destination_port()));
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
        assert!(port_range.contains(80));
        assert!(port_range.contains(443));
        assert!(!port_range.contains(79));
        assert!(!port_range.contains(444));
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

        let source_port_range = Negated::new(PortRange::new(1024, 2048).unwrap(), false);
        let port_range = Negated::new(port_range, false);
        let flags = Negated::new(flags, false);
        let tcp = TcpMatch::new()
            .with_source_port(source_port_range)
            .with_destination_port(port_range)
            .with_flags(flags);
        assert_eq!(tcp.source_port_range(), Some(source_port_range));
        assert_eq!(tcp.destination_port_range(), Some(port_range));
        assert_eq!(tcp.flags_match(), Some(flags));

        let udp = UdpMatch::new()
            .with_source_port(source_port_range)
            .with_destination_port(port_range);
        assert_eq!(udp.source_port_range(), Some(source_port_range));
        assert_eq!(udp.destination_port_range(), Some(port_range));

        let exact = InterfaceMatch::exact("eth0".into()).unwrap();
        assert_eq!(exact.name(), "eth0");
        assert!(!exact.is_prefix());
        let prefix = InterfaceMatch::prefix("veth".into()).unwrap();
        assert_eq!(prefix.name(), "veth");
        assert!(prefix.is_prefix());
        assert_eq!(
            InterfaceMatch::exact("X".repeat(MAX_INTERFACE_NAME_LEN + 1)),
            None
        );
    }
}
