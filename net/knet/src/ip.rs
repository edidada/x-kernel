// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IP address, CIDR, and endpoint types used by the in-kernel stack.

use core::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    str::FromStr,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Ipv4Address([u8; 4]);

impl Ipv4Address {
    pub(crate) const BROADCAST: Self = Self([255, 255, 255, 255]);
    pub(crate) const UNSPECIFIED: Self = Self([0, 0, 0, 0]);

    pub(crate) const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self([a, b, c, d])
    }

    pub(crate) const fn from_octets(octets: [u8; 4]) -> Self {
        Self(octets)
    }

    pub(crate) const fn octets(self) -> [u8; 4] {
        self.0
    }

    pub(crate) fn is_unspecified(self) -> bool {
        self == Self::UNSPECIFIED
    }

    pub(crate) fn is_broadcast(self) -> bool {
        self == Self::BROADCAST
    }

    pub(crate) fn is_multicast(self) -> bool {
        (224..=239).contains(&self.0[0])
    }

    pub(crate) fn is_unicast(self) -> bool {
        !(self.is_unspecified() || self.is_broadcast() || self.is_multicast())
    }
}

impl From<Ipv4Addr> for Ipv4Address {
    fn from(addr: Ipv4Addr) -> Self {
        Self(addr.octets())
    }
}

impl From<Ipv4Address> for Ipv4Addr {
    fn from(addr: Ipv4Address) -> Self {
        Self::from(addr.0)
    }
}

impl FromStr for Ipv4Address {
    type Err = <Ipv4Addr as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ipv4Addr::from_str(s).map(Self::from)
    }
}

impl fmt::Display for Ipv4Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ipv4Addr::from(*self).fmt(f)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Ipv6Address([u8; 16]);

impl Ipv6Address {
    pub(crate) const ALL_NODES_LINK_LOCAL: Self =
        Self([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    pub(crate) const LOCALHOST: Self = Self([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    pub(crate) const UNSPECIFIED: Self = Self([0; 16]);

    pub(crate) const fn from_octets(octets: [u8; 16]) -> Self {
        Self(octets)
    }

    pub(crate) const fn octets(self) -> [u8; 16] {
        self.0
    }

    pub(crate) fn is_unspecified(self) -> bool {
        self == Self::UNSPECIFIED
    }

    pub(crate) fn is_loopback(self) -> bool {
        self == Self::LOCALHOST
    }

    pub(crate) fn is_multicast(self) -> bool {
        self.0[0] == 0xff
    }

    pub(crate) fn is_link_local(self) -> bool {
        self.0[0] == 0xfe && (self.0[1] & 0xc0) == 0x80
    }

    /// Derives a link-local address from a 48-bit MAC via modified EUI-64.
    pub(crate) fn link_local_from_mac(mac: [u8; 6]) -> Self {
        let mut octets = [0u8; 16];
        octets[0] = 0xfe;
        octets[1] = 0x80;
        octets[8] = mac[0] ^ 0x02;
        octets[9] = mac[1];
        octets[10] = mac[2];
        octets[11] = 0xff;
        octets[12] = 0xfe;
        octets[13] = mac[3];
        octets[14] = mac[4];
        octets[15] = mac[5];
        Self(octets)
    }

    /// Solicited-node multicast address for this unicast address (RFC 4291).
    pub(crate) fn solicited_node_multicast(self) -> Self {
        let mut octets = [0u8; 16];
        octets[0] = 0xff;
        octets[1] = 0x02;
        octets[11] = 0x01;
        octets[12] = 0xff;
        octets[13] = self.0[13];
        octets[14] = self.0[14];
        octets[15] = self.0[15];
        Self(octets)
    }

    /// Maps an IPv6 multicast address to the Ethernet multicast MAC.
    pub(crate) fn multicast_mac(self) -> [u8; 6] {
        [0x33, 0x33, self.0[12], self.0[13], self.0[14], self.0[15]]
    }
}

impl From<Ipv6Addr> for Ipv6Address {
    fn from(addr: Ipv6Addr) -> Self {
        Self::from_octets(addr.octets())
    }
}

impl From<Ipv6Address> for Ipv6Addr {
    fn from(addr: Ipv6Address) -> Self {
        Self::from(addr.octets())
    }
}

impl fmt::Display for Ipv6Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ipv6Addr::from(*self).fmt(f)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum IpAddress {
    Ipv4(Ipv4Address),
    Ipv6(Ipv6Address),
}

impl IpAddress {
    pub(crate) fn is_unspecified(self) -> bool {
        match self {
            Self::Ipv4(addr) => addr.is_unspecified(),
            Self::Ipv6(addr) => addr.is_unspecified(),
        }
    }

    pub(crate) fn is_broadcast(self) -> bool {
        matches!(self, Self::Ipv4(addr) if addr.is_broadcast())
    }

    pub(crate) fn is_multicast(self) -> bool {
        match self {
            Self::Ipv4(addr) => addr.is_multicast(),
            Self::Ipv6(addr) => addr.is_multicast(),
        }
    }
}

impl From<Ipv4Address> for IpAddress {
    fn from(addr: Ipv4Address) -> Self {
        Self::Ipv4(addr)
    }
}

impl From<Ipv6Address> for IpAddress {
    fn from(addr: Ipv6Address) -> Self {
        Self::Ipv6(addr)
    }
}

impl From<IpAddr> for IpAddress {
    fn from(addr: IpAddr) -> Self {
        match addr {
            IpAddr::V4(addr) => Self::Ipv4(addr.into()),
            IpAddr::V6(addr) => Self::Ipv6(addr.into()),
        }
    }
}

impl From<IpAddress> for IpAddr {
    fn from(addr: IpAddress) -> Self {
        match addr {
            IpAddress::Ipv4(addr) => Self::V4(addr.into()),
            IpAddress::Ipv6(addr) => Self::V6(addr.into()),
        }
    }
}

impl FromStr for IpAddress {
    type Err = <IpAddr as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        IpAddr::from_str(s).map(Self::from)
    }
}

impl fmt::Display for IpAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ipv4(addr) => addr.fmt(f),
            Self::Ipv6(addr) => addr.fmt(f),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Ipv4Cidr {
    address: Ipv4Address,
    prefix_len: u8,
}

impl Ipv4Cidr {
    pub(crate) const fn new(address: Ipv4Address, prefix_len: u8) -> Self {
        match Self::try_new(address, prefix_len) {
            Some(cidr) => cidr,
            None => panic!("IPv4 prefix length must not exceed 32"),
        }
    }

    pub(crate) const fn try_new(address: Ipv4Address, prefix_len: u8) -> Option<Self> {
        if prefix_len <= 32 {
            Some(Self {
                address,
                prefix_len,
            })
        } else {
            None
        }
    }

    pub(crate) const fn address(self) -> Ipv4Address {
        self.address
    }

    pub(crate) const fn prefix_len(self) -> u8 {
        self.prefix_len
    }

    pub(crate) fn network(self) -> Self {
        let mask = prefix_mask(self.prefix_len);
        let addr = u32::from_be_bytes(self.address.octets()) & mask;
        Self::new(
            Ipv4Address::from_octets(addr.to_be_bytes()),
            self.prefix_len,
        )
    }

    pub(crate) fn broadcast(self) -> Option<Ipv4Address> {
        let mask = prefix_mask(self.prefix_len);
        let addr = u32::from_be_bytes(self.address.octets());
        Some(Ipv4Address::from_octets((addr | !mask).to_be_bytes()))
    }

    pub(crate) fn is_directed_broadcast(self, addr: Ipv4Address) -> bool {
        self.prefix_len < 31 && self.broadcast() == Some(addr)
    }

    pub(crate) fn contains_addr(self, addr: &IpAddress) -> bool {
        let IpAddress::Ipv4(addr) = *addr else {
            return false;
        };
        let mask = prefix_mask(self.prefix_len);
        let lhs = u32::from_be_bytes(self.address.octets()) & mask;
        let rhs = u32::from_be_bytes(addr.octets()) & mask;
        lhs == rhs
    }
}

impl From<Ipv4Cidr> for IpCidr {
    fn from(cidr: Ipv4Cidr) -> Self {
        Self::Ipv4(cidr)
    }
}

impl fmt::Display for Ipv4Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix_len)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Ipv6Cidr {
    address: Ipv6Address,
    prefix_len: u8,
}

impl Ipv6Cidr {
    pub(crate) const fn new(address: Ipv6Address, prefix_len: u8) -> Self {
        match Self::try_new(address, prefix_len) {
            Some(cidr) => cidr,
            None => panic!("IPv6 prefix length must not exceed 128"),
        }
    }

    pub(crate) const fn try_new(address: Ipv6Address, prefix_len: u8) -> Option<Self> {
        if prefix_len <= 128 {
            Some(Self {
                address,
                prefix_len,
            })
        } else {
            None
        }
    }

    pub(crate) const fn address(self) -> Ipv6Address {
        self.address
    }

    pub(crate) const fn prefix_len(self) -> u8 {
        self.prefix_len
    }

    pub(crate) fn contains_addr(self, addr: &IpAddress) -> bool {
        let IpAddress::Ipv6(addr) = *addr else {
            return false;
        };
        ipv6_prefix_equal(self.address.octets(), addr.octets(), self.prefix_len)
    }
}

impl From<Ipv6Cidr> for IpCidr {
    fn from(cidr: Ipv6Cidr) -> Self {
        Self::Ipv6(cidr)
    }
}

impl fmt::Display for Ipv6Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix_len)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum IpCidr {
    Ipv4(Ipv4Cidr),
    Ipv6(Ipv6Cidr),
}

impl IpCidr {
    pub(crate) fn address(self) -> IpAddress {
        match self {
            Self::Ipv4(cidr) => IpAddress::Ipv4(cidr.address()),
            Self::Ipv6(cidr) => IpAddress::Ipv6(cidr.address()),
        }
    }

    pub(crate) fn prefix_len(self) -> u8 {
        match self {
            Self::Ipv4(cidr) => cidr.prefix_len(),
            Self::Ipv6(cidr) => cidr.prefix_len(),
        }
    }

    pub(crate) fn contains_addr(self, addr: &IpAddress) -> bool {
        match self {
            Self::Ipv4(cidr) => cidr.contains_addr(addr),
            Self::Ipv6(cidr) => cidr.contains_addr(addr),
        }
    }
}

/// An IP address and transport port used by the native stack.
///
/// IPv6 flow information is intentionally not represented because the stack
/// does not implement `IPV6_FLOWINFO_SEND` or flow-label routing. Conversions
/// from [`SocketAddrV6`] therefore normalize `flowinfo` to zero.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct IpEndpoint {
    pub(crate) addr: IpAddress,
    pub(crate) port: u16,
    pub(crate) scope_id: u32,
}

impl From<SocketAddr> for IpEndpoint {
    fn from(addr: SocketAddr) -> Self {
        match addr {
            SocketAddr::V4(addr) => Self::from(addr),
            SocketAddr::V6(addr) => Self::from(addr),
        }
    }
}

impl From<SocketAddrV4> for IpEndpoint {
    fn from(addr: SocketAddrV4) -> Self {
        Self {
            addr: IpAddress::Ipv4((*addr.ip()).into()),
            port: addr.port(),
            scope_id: 0,
        }
    }
}

impl From<SocketAddrV6> for IpEndpoint {
    fn from(addr: SocketAddrV6) -> Self {
        Self {
            addr: IpAddress::Ipv6((*addr.ip()).into()),
            port: addr.port(),
            scope_id: addr.scope_id(),
        }
    }
}

impl From<IpEndpoint> for SocketAddr {
    fn from(endpoint: IpEndpoint) -> Self {
        match endpoint.addr {
            IpAddress::Ipv4(addr) => Self::V4(SocketAddrV4::new(addr.into(), endpoint.port)),
            IpAddress::Ipv6(addr) => Self::V6(SocketAddrV6::new(
                addr.into(),
                endpoint.port,
                0,
                endpoint.scope_id,
            )),
        }
    }
}

impl fmt::Display for IpEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.addr {
            IpAddress::Ipv4(addr) => write!(f, "{}:{}", addr, self.port),
            IpAddress::Ipv6(addr) => {
                SocketAddrV6::new(addr.into(), self.port, 0, self.scope_id).fmt(f)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct IpListenEndpoint {
    pub(crate) addr: Option<IpAddress>,
    pub(crate) port: u16,
    pub(crate) scope_id: u32,
}

impl fmt::Display for IpListenEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.addr {
            Some(IpAddress::Ipv4(addr)) => write!(f, "{}:{}", addr, self.port),
            Some(IpAddress::Ipv6(addr)) => {
                SocketAddrV6::new(addr.into(), self.port, 0, self.scope_id).fmt(f)
            }
            None if self.scope_id != 0 => {
                SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, self.port, 0, self.scope_id).fmt(f)
            }
            None => write!(f, "*:{}", self.port),
        }
    }
}

fn prefix_mask(prefix_len: u8) -> u32 {
    match prefix_len {
        0 => 0,
        1..=32 => u32::MAX << (32 - prefix_len),
        _ => u32::MAX,
    }
}

fn ipv6_prefix_equal(lhs: [u8; 16], rhs: [u8; 16], prefix_len: u8) -> bool {
    if prefix_len == 0 {
        return true;
    }
    if prefix_len == 128 {
        return lhs == rhs;
    }
    let full_bytes = (prefix_len / 8) as usize;
    let rem_bits = prefix_len % 8;
    if lhs[..full_bytes] != rhs[..full_bytes] {
        return false;
    }
    if rem_bits == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rem_bits);
    (lhs[full_bytes] & mask) == (rhs[full_bytes] & mask)
}

#[cfg(unittest)]
mod tests {
    use alloc::string::ToString;

    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_ip_address_and_cidr_helpers() {
        let addr = Ipv4Address::new(192, 0, 2, 1);
        let cidr = Ipv4Cidr::new(addr, 24);
        let ip_cidr = IpCidr::from(cidr);

        assert_eq!(cidr.address(), addr);
        assert_eq!(cidr.prefix_len(), 24);
        assert_eq!(cidr.broadcast(), Some(Ipv4Address::new(192, 0, 2, 255)));
        assert!(cidr.contains_addr(&IpAddress::from(addr)));
        assert_eq!(ip_cidr.address(), IpAddress::from(addr));
        assert_eq!(ip_cidr.prefix_len(), 24);
        assert!(ip_cidr.contains_addr(&IpAddress::from(addr)));
        assert!(IpAddress::from(Ipv4Address::UNSPECIFIED).is_unspecified());
        assert!(IpAddress::from(Ipv4Address::BROADCAST).is_broadcast());
    }

    #[def_test]
    fn test_cidr_prefix_boundaries() {
        let ipv4 = Ipv4Address::new(192, 0, 2, 1);
        let ipv4_default = Ipv4Cidr::new(ipv4, 0);
        let ipv4_host = Ipv4Cidr::new(ipv4, 32);
        assert!(ipv4_default.contains_addr(&IpAddress::Ipv4(Ipv4Address::new(203, 0, 113, 9))));
        assert!(ipv4_host.contains_addr(&IpAddress::Ipv4(ipv4)));
        assert!(!ipv4_host.contains_addr(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 2))));
        assert!(Ipv4Cidr::try_new(ipv4, 33).is_none());

        let ipv6 =
            Ipv6Address::from_octets([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let ipv6_next =
            Ipv6Address::from_octets([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let ipv6_outside_127 =
            Ipv6Address::from_octets([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        assert!(Ipv6Cidr::new(ipv6, 0).contains_addr(&IpAddress::Ipv6(Ipv6Address::LOCALHOST)));
        assert!(Ipv6Cidr::new(ipv6, 64).contains_addr(&IpAddress::Ipv6(ipv6_next)));
        assert!(Ipv6Cidr::new(ipv6, 127).contains_addr(&IpAddress::Ipv6(ipv6_next)));
        assert!(!Ipv6Cidr::new(ipv6, 127).contains_addr(&IpAddress::Ipv6(ipv6_outside_127)));
        assert!(Ipv6Cidr::new(ipv6, 128).contains_addr(&IpAddress::Ipv6(ipv6)));
        assert!(!Ipv6Cidr::new(ipv6, 128).contains_addr(&IpAddress::Ipv6(ipv6_next)));
        assert!(Ipv6Cidr::try_new(ipv6, 129).is_none());
        assert!(Ipv6Cidr::try_new(ipv6, u8::MAX).is_none());
    }

    #[def_test]
    fn test_ipv6_endpoint_display_and_conversion() {
        let octets = [1; 16];
        let addr = Ipv6Address::from_octets(octets);
        let endpoint = IpEndpoint {
            addr: IpAddress::from(addr),
            port: 8080,
            scope_id: 7,
        };
        let listen_endpoint = IpListenEndpoint {
            addr: Some(IpAddress::from(addr)),
            port: 8080,
            scope_id: 7,
        };
        let cidr = Ipv6Cidr::new(addr, 64);
        let link_local = Ipv6Address::link_local_from_mac([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        let socket_addr = SocketAddrV6::new(addr.into(), 8080, 0x1234, 7);
        let converted = IpEndpoint::from(socket_addr);
        let round_trip = SocketAddr::from(converted);

        assert_eq!(addr.octets(), octets);
        assert!(!addr.is_unspecified());
        assert!(Ipv6Address::UNSPECIFIED.is_unspecified());
        assert!(Ipv6Address::LOCALHOST.is_loopback());
        assert!(Ipv6Address::ALL_NODES_LINK_LOCAL.is_multicast());
        assert!(link_local.is_link_local());
        assert!(cidr.contains_addr(&IpAddress::from(addr)));
        assert_eq!(IpCidr::from(cidr).prefix_len(), 64);
        assert_eq!(
            endpoint.to_string(),
            "[101:101:101:101:101:101:101:101%7]:8080"
        );
        assert_eq!(listen_endpoint.to_string(), endpoint.to_string());
        assert_eq!(
            IpListenEndpoint {
                addr: None,
                port: 8080,
                scope_id: 7,
            }
            .to_string(),
            "[::%7]:8080"
        );
        assert_eq!(converted.scope_id, 7);
        assert_eq!(
            round_trip,
            SocketAddr::V6(SocketAddrV6::new(addr.into(), 8080, 0, 7))
        );
        assert_eq!(addr.solicited_node_multicast().octets()[13..], [1, 1, 1]);
    }

    #[def_test]
    fn test_ipv4_endpoint_display() {
        let endpoint = IpEndpoint {
            addr: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1)),
            port: 8080,
            scope_id: 0,
        };
        let listen_endpoint = IpListenEndpoint {
            addr: Some(endpoint.addr),
            port: endpoint.port,
            scope_id: 0,
        };

        assert_eq!(endpoint.to_string(), "192.168.1.1:8080");
        assert_eq!(listen_endpoint.to_string(), endpoint.to_string());
    }
}
