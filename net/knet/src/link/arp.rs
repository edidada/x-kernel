// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Checked ARP/IPv4 packet parsing and construction.

use zerocopy::{
    FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned, byteorder::network_endian::U16,
};

use crate::{
    ip::{Ipv4Address, Ipv4Cidr},
    link::wire::MacAddress,
};

const ARP_ETHERNET_IPV4_LEN: usize = 28;
const ARP_HARDWARE_ETHERNET: u16 = 1;
const ARP_PROTOCOL_IPV4: u16 = 0x0800;
const ARP_HARDWARE_LEN_ETHERNET: u8 = 6;
const ARP_PROTOCOL_LEN_IPV4: u8 = 4;

/// Ethernet/IPv4 ARP operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArpOperation {
    Request,
    Reply,
    Unknown(u16),
}

impl ArpOperation {
    fn from_u16(value: u16) -> Self {
        match value {
            1 => Self::Request,
            2 => Self::Reply,
            other => Self::Unknown(other),
        }
    }

    fn as_u16(self) -> u16 {
        match self {
            Self::Request => 1,
            Self::Reply => 2,
            Self::Unknown(value) => value,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned)]
struct ArpIpv4Header {
    hardware_type: U16,
    protocol_type: U16,
    hardware_len: u8,
    protocol_len: u8,
    operation: U16,
    source_hardware_addr: [u8; 6],
    source_protocol_addr: [u8; 4],
    target_hardware_addr: [u8; 6],
    target_protocol_addr: [u8; 4],
}

/// Parsed Ethernet/IPv4 ARP packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArpIpv4Packet {
    pub(crate) operation: ArpOperation,
    pub(crate) source_hardware_addr: MacAddress,
    pub(crate) source_protocol_addr: Ipv4Address,
    pub(crate) target_hardware_addr: MacAddress,
    pub(crate) target_protocol_addr: Ipv4Address,
}

impl ArpIpv4Packet {
    pub(crate) const LEN: usize = ARP_ETHERNET_IPV4_LEN;

    pub(crate) fn parse(data: &[u8]) -> Option<Self> {
        let (header, _) = ArpIpv4Header::ref_from_prefix(data).ok()?;
        if header.hardware_type.get() != ARP_HARDWARE_ETHERNET
            || header.protocol_type.get() != ARP_PROTOCOL_IPV4
            || header.hardware_len != ARP_HARDWARE_LEN_ETHERNET
            || header.protocol_len != ARP_PROTOCOL_LEN_IPV4
        {
            return None;
        }

        Some(Self {
            operation: ArpOperation::from_u16(header.operation.get()),
            source_hardware_addr: MacAddress(header.source_hardware_addr),
            source_protocol_addr: Ipv4Address::from_octets(header.source_protocol_addr),
            target_hardware_addr: MacAddress(header.target_hardware_addr),
            target_protocol_addr: Ipv4Address::from_octets(header.target_protocol_addr),
        })
    }

    pub(crate) fn emit(self, data: &mut [u8]) -> Option<()> {
        if data.len() < ARP_ETHERNET_IPV4_LEN {
            return None;
        }

        let header = ArpIpv4Header {
            hardware_type: U16::new(ARP_HARDWARE_ETHERNET),
            protocol_type: U16::new(ARP_PROTOCOL_IPV4),
            hardware_len: ARP_HARDWARE_LEN_ETHERNET,
            protocol_len: ARP_PROTOCOL_LEN_IPV4,
            operation: U16::new(self.operation.as_u16()),
            source_hardware_addr: self.source_hardware_addr.0,
            source_protocol_addr: self.source_protocol_addr.octets(),
            target_hardware_addr: self.target_hardware_addr.0,
            target_protocol_addr: self.target_protocol_addr.octets(),
        };
        data[..ARP_ETHERNET_IPV4_LEN].copy_from_slice(header.as_bytes());
        Some(())
    }
}

/// The validated information needed by the device and neighbor resolver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArpReceiveEvent {
    pub(crate) operation: ArpOperation,
    pub(crate) source_hardware_addr: MacAddress,
    pub(crate) source_protocol_addr: Ipv4Address,
    pub(crate) target_protocol_addr: Ipv4Address,
    pub(crate) is_host_directed_reply: bool,
}

impl ArpReceiveEvent {
    pub(crate) fn should_learn_source(self) -> bool {
        !self.source_protocol_addr.is_unspecified()
    }

    pub(crate) fn reply(self, local_hardware_addr: MacAddress) -> Option<ArpIpv4Packet> {
        (self.operation == ArpOperation::Request).then_some(ArpIpv4Packet {
            operation: ArpOperation::Reply,
            source_hardware_addr: local_hardware_addr,
            source_protocol_addr: self.target_protocol_addr,
            target_hardware_addr: self.source_hardware_addr,
            target_protocol_addr: self.source_protocol_addr,
        })
    }
}

/// Parses an ARP frame and applies the Ethernet/IP validity rules shared by
/// all Ethernet devices.
pub(crate) fn parse_receive(
    frame_destination: MacAddress,
    local_hardware_addr: MacAddress,
    local_ipv4_addrs: &[Ipv4Cidr],
    data: &[u8],
) -> Option<ArpReceiveEvent> {
    let packet = ArpIpv4Packet::parse(data)?;
    if matches!(packet.operation, ArpOperation::Unknown(_)) {
        return None;
    }

    let target_is_unicast = packet.target_hardware_addr != MacAddress::ZERO
        && !packet.target_hardware_addr.is_broadcast();
    if target_is_unicast && packet.target_hardware_addr != local_hardware_addr {
        return None;
    }
    if !packet.source_hardware_addr.is_unicast()
        || packet.source_protocol_addr.is_broadcast()
        || packet.source_protocol_addr.is_multicast()
    {
        return None;
    }
    // Linux 6.8 `arp_process` answers RFC 2131 DAD probes (`sip == 0`) that
    // target a local address, then skips neighbour learning.
    if packet.source_protocol_addr.is_unspecified() && packet.operation != ArpOperation::Request {
        return None;
    }
    if !local_ipv4_addrs
        .iter()
        .any(|addr| addr.address() == packet.target_protocol_addr)
    {
        return None;
    }

    let is_host_directed_reply =
        packet.operation == ArpOperation::Reply && frame_destination == local_hardware_addr;
    Some(ArpReceiveEvent {
        operation: packet.operation,
        source_hardware_addr: packet.source_hardware_addr,
        source_protocol_addr: packet.source_protocol_addr,
        target_protocol_addr: packet.target_protocol_addr,
        is_host_directed_reply,
    })
}

pub(crate) fn build_request(
    source_hardware_addr: MacAddress,
    source_protocol_addr: Ipv4Address,
    target_protocol_addr: Ipv4Address,
) -> ArpIpv4Packet {
    ArpIpv4Packet {
        operation: ArpOperation::Request,
        source_hardware_addr,
        source_protocol_addr,
        target_hardware_addr: MacAddress::BROADCAST,
        target_protocol_addr,
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    const LOCAL_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 1]);
    const REMOTE_MAC: MacAddress = MacAddress([0x02, 0, 0, 0, 0, 2]);
    const LOCAL_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
    const REMOTE_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);

    #[def_test]
    fn arp_ipv4_parse_and_emit() {
        let packet = ArpIpv4Packet {
            operation: ArpOperation::Reply,
            source_hardware_addr: REMOTE_MAC,
            source_protocol_addr: REMOTE_IP,
            target_hardware_addr: LOCAL_MAC,
            target_protocol_addr: LOCAL_IP,
        };
        let mut data = [0u8; ArpIpv4Packet::LEN];
        packet.emit(&mut data).expect("ARP buffer is large enough");

        assert_eq!(ArpIpv4Packet::parse(&data), Some(packet));
    }

    #[def_test]
    fn receive_event_accepts_only_local_target() {
        let packet = build_request(REMOTE_MAC, REMOTE_IP, LOCAL_IP);
        let mut data = [0u8; ArpIpv4Packet::LEN];
        packet.emit(&mut data).expect("ARP buffer is large enough");
        let addrs = [Ipv4Cidr::new(LOCAL_IP, 24)];

        let event = parse_receive(LOCAL_MAC, LOCAL_MAC, &addrs, &data)
            .expect("request for a local IPv4 address is valid");
        assert!(!event.is_host_directed_reply);
        assert_eq!(
            event.reply(LOCAL_MAC),
            Some(ArpIpv4Packet {
                operation: ArpOperation::Reply,
                source_hardware_addr: LOCAL_MAC,
                source_protocol_addr: LOCAL_IP,
                target_hardware_addr: REMOTE_MAC,
                target_protocol_addr: REMOTE_IP,
            })
        );

        let packet = build_request(REMOTE_MAC, REMOTE_IP, Ipv4Address::new(10, 0, 2, 99));
        packet.emit(&mut data).expect("ARP buffer is large enough");
        assert!(parse_receive(LOCAL_MAC, LOCAL_MAC, &addrs, &data).is_none());
    }

    #[def_test]
    fn receive_event_replies_to_dad_probe_without_learning_source() {
        let packet = build_request(REMOTE_MAC, Ipv4Address::UNSPECIFIED, LOCAL_IP);
        let mut data = [0u8; ArpIpv4Packet::LEN];
        packet.emit(&mut data).expect("ARP buffer is large enough");
        let addrs = [Ipv4Cidr::new(LOCAL_IP, 24)];

        let event = parse_receive(LOCAL_MAC, LOCAL_MAC, &addrs, &data)
            .expect("DAD request for a local IPv4 address is valid");
        assert!(!event.should_learn_source());
        assert_eq!(
            event.reply(LOCAL_MAC),
            Some(ArpIpv4Packet {
                operation: ArpOperation::Reply,
                source_hardware_addr: LOCAL_MAC,
                source_protocol_addr: LOCAL_IP,
                target_hardware_addr: REMOTE_MAC,
                target_protocol_addr: Ipv4Address::UNSPECIFIED,
            })
        );

        let reply = ArpIpv4Packet {
            operation: ArpOperation::Reply,
            source_hardware_addr: REMOTE_MAC,
            source_protocol_addr: Ipv4Address::UNSPECIFIED,
            target_hardware_addr: LOCAL_MAC,
            target_protocol_addr: LOCAL_IP,
        };
        reply.emit(&mut data).expect("ARP buffer is large enough");
        assert!(parse_receive(LOCAL_MAC, LOCAL_MAC, &addrs, &data).is_none());
    }

    #[def_test]
    fn directed_reply_is_reachable_and_request_source_is_preserved() {
        let packet = ArpIpv4Packet {
            operation: ArpOperation::Reply,
            source_hardware_addr: REMOTE_MAC,
            source_protocol_addr: REMOTE_IP,
            target_hardware_addr: LOCAL_MAC,
            target_protocol_addr: LOCAL_IP,
        };
        let mut data = [0u8; ArpIpv4Packet::LEN];
        packet.emit(&mut data).expect("ARP buffer is large enough");
        let addrs = [Ipv4Cidr::new(LOCAL_IP, 24)];

        let event =
            parse_receive(LOCAL_MAC, LOCAL_MAC, &addrs, &data).expect("directed reply is valid");
        assert!(event.is_host_directed_reply);

        let request = build_request(LOCAL_MAC, LOCAL_IP, REMOTE_IP);
        assert_eq!(request.source_protocol_addr, LOCAL_IP);
        assert_eq!(request.target_protocol_addr, REMOTE_IP);
    }
}
