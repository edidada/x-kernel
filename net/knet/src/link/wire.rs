// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Link-layer wire format helpers.

use core::fmt;

use zerocopy::{
    FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned, byteorder::network_endian::U16,
};

/// Ethernet header length in bytes.
pub(crate) const ETHERNET_HEADER_LEN: usize = 14;

#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned)]
struct EthernetHeader {
    dst_addr: [u8; 6],
    src_addr: [u8; 6],
    ethertype: U16,
}

/// A six-octet Ethernet address.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct MacAddress(pub(crate) [u8; 6]);

impl MacAddress {
    pub(crate) const BROADCAST: Self = Self([0xff; 6]);
    pub(crate) const ZERO: Self = Self([0; 6]);

    pub(crate) const fn bytes(self) -> [u8; 6] {
        self.0
    }

    pub(crate) fn is_broadcast(self) -> bool {
        self == Self::BROADCAST
    }

    pub(crate) fn is_multicast(self) -> bool {
        self.0[0] & 1 != 0
    }

    pub(crate) fn is_unicast(self) -> bool {
        !self.is_broadcast() && !self.is_multicast()
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.0;
        write!(
            f,
            "{:02x}-{:02x}-{:02x}-{:02x}-{:02x}-{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
        )
    }
}

/// Ethernet DIX EtherType.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum EtherType {
    Ipv4,
    Arp,
    Ipv6,
    Other(u16),
}

impl EtherType {
    pub(crate) fn from_u16(value: u16) -> Self {
        match value {
            0x0800 => Self::Ipv4,
            0x0806 => Self::Arp,
            0x86dd => Self::Ipv6,
            other => Self::Other(other),
        }
    }

    pub(crate) fn as_u16(self) -> u16 {
        match self {
            Self::Ipv4 => 0x0800,
            Self::Arp => 0x0806,
            Self::Ipv6 => 0x86dd,
            Self::Other(value) => value,
        }
    }
}

impl From<EtherType> for u16 {
    fn from(value: EtherType) -> Self {
        value.as_u16()
    }
}

/// A checked reference to an Ethernet frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EthernetFrameRef<'a> {
    header: &'a EthernetHeader,
    payload: &'a [u8],
}

impl<'a> EthernetFrameRef<'a> {
    pub(crate) fn new_checked(data: &'a [u8]) -> Option<Self> {
        let (header, payload) = EthernetHeader::ref_from_prefix(data).ok()?;
        Some(Self { header, payload })
    }

    pub(crate) fn dst_addr(self) -> MacAddress {
        MacAddress(self.header.dst_addr)
    }

    pub(crate) fn src_addr(self) -> MacAddress {
        MacAddress(self.header.src_addr)
    }

    pub(crate) fn ethertype(self) -> EtherType {
        EtherType::from_u16(self.header.ethertype.get())
    }

    pub(crate) fn payload(self) -> &'a [u8] {
        self.payload
    }
}

pub(crate) fn emit_ethernet_header(
    frame: &mut [u8],
    dst_addr: MacAddress,
    src_addr: MacAddress,
    ethertype: EtherType,
) -> Option<&mut [u8]> {
    if frame.len() < ETHERNET_HEADER_LEN {
        return None;
    }

    let header = EthernetHeader {
        dst_addr: dst_addr.0,
        src_addr: src_addr.0,
        ethertype: U16::new(ethertype.as_u16()),
    };
    frame[..ETHERNET_HEADER_LEN].copy_from_slice(header.as_bytes());
    Some(&mut frame[ETHERNET_HEADER_LEN..])
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    const LOCAL_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 1]);
    const REMOTE_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 2]);

    #[def_test]
    fn test_ethernet_frame_parse_and_emit() {
        let mut frame = [0u8; ETHERNET_HEADER_LEN + 2];
        let payload = emit_ethernet_header(&mut frame, LOCAL_MAC, REMOTE_MAC, EtherType::Ipv4)
            .expect("Ethernet header buffer is large enough");
        payload.copy_from_slice(&[1, 2]);

        let frame_ref =
            EthernetFrameRef::new_checked(&frame).expect("Ethernet frame length is valid");
        assert_eq!(frame_ref.dst_addr(), LOCAL_MAC);
        assert_eq!(frame_ref.src_addr(), REMOTE_MAC);
        assert_eq!(frame_ref.ethertype(), EtherType::Ipv4);
        assert_eq!(frame_ref.payload(), &[1, 2]);
    }
}
