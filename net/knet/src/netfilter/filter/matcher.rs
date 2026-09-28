// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Value types for fixed IPv4 and protocol-specific filter conditions.
#![expect(dead_code, reason = "filter rules are introduced in a later stage")]

use alloc::string::String;

use crate::ip::Ipv4Cidr;

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
        let max_len = MAX_INTERFACE_NAME_LEN - usize::from(is_prefix);
        (name.len() <= max_len).then_some(Self { name, is_prefix })
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

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn match_value_constructors_reject_invalid_ranges_and_names() {
        let ports = PortRange::new(80, 443).unwrap();
        assert!(ports.contains(80));
        assert!(ports.contains(443));
        assert_eq!(PortRange::new(443, 80), None);

        assert!(TcpFlagsMatch::new(0x12, 0x02).is_some());
        assert_eq!(TcpFlagsMatch::new(0x02, 0x12), None);
        assert!(IcmpMatch::new(Some(3), 0, 15).is_some());
        assert_eq!(IcmpMatch::new(Some(3), 15, 0), None);

        assert!(InterfaceMatch::exact("x".repeat(MAX_INTERFACE_NAME_LEN)).is_some());
        assert!(InterfaceMatch::exact("x".repeat(MAX_INTERFACE_NAME_LEN + 1)).is_none());
        assert!(InterfaceMatch::prefix("x".repeat(MAX_INTERFACE_NAME_LEN - 1)).is_some());
        assert!(InterfaceMatch::prefix("x".repeat(MAX_INTERFACE_NAME_LEN)).is_none());
    }
}
