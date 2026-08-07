// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Neighbor Discovery Protocol (NDP) helpers.

use alloc::{vec, vec::Vec};

use etherparse::{Icmpv6Header, Icmpv6Slice, Icmpv6Type, icmpv6::NeighborAdvertisementHeader};

use crate::{
    ip::{Ipv6Address, Ipv6Cidr},
    ipv6::{self, IPV6_HEADER_LEN, Ipv6Header, PROTOCOL_ICMPV6},
    wire::MacAddress,
};

const NDP_HOP_LIMIT: u8 = 255;
const NDP_OPTION_SOURCE_LLA: u8 = 1;
const NDP_OPTION_TARGET_LLA: u8 = 2;
const NDP_LLA_OPTION_LEN: usize = 8;
const NDP_TARGET_LEN: usize = 16;
// etherparse's NeighborSolicitation/NeighborAdvertisement header already
// covers the 4-byte Reserved/R/S/O field, so the payload only carries the
// 16-byte target plus the 8-byte LLA option (RFC 4861 §4.3/§4.4).
const NDP_BASE_PAYLOAD_LEN: usize = NDP_TARGET_LEN + NDP_LLA_OPTION_LEN;

/// Result of processing an inbound NDP message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NdpAction {
    /// Learn a neighbor mapping from an advertisement or solicitation.
    LearnNeighbor { addr: Ipv6Address, mac: MacAddress },
    /// A solicited-node NS targeted one of our addresses; reply with NA.
    ReplyAdvertisement {
        target: Ipv6Address,
        dst: Ipv6Address,
        solicited: bool,
    },
}

/// Parsed inbound NDP message with all validation already applied.
pub(crate) struct NdpInput {
    /// Neighbor mapping extracted from a solicitation's source LLA option.
    pub(crate) learn: Option<(Ipv6Address, MacAddress)>,
    /// Action to take for the parsed message.
    pub(crate) action: Option<NdpAction>,
}

/// Builds an IPv6 Neighbor Solicitation packet for the given target.
pub(crate) fn build_neighbor_solicitation(
    src_addr: Ipv6Address,
    target: Ipv6Address,
    src_mac: MacAddress,
) -> Option<Vec<u8>> {
    let dst = target.solicited_node_multicast();
    let mut icmp_payload = vec![0u8; NDP_BASE_PAYLOAD_LEN];
    icmp_payload[..NDP_TARGET_LEN].copy_from_slice(&target.octets());
    write_lla_option(
        &mut icmp_payload[NDP_TARGET_LEN..],
        NDP_OPTION_SOURCE_LLA,
        src_mac,
    )?;

    let icmp_header = Icmpv6Header::with_checksum(
        Icmpv6Type::NeighborSolicitation,
        src_addr.octets(),
        dst.octets(),
        &icmp_payload,
    )
    .ok()?;

    assemble_ndp_packet(src_addr, dst, &icmp_header, &icmp_payload)
}

/// Builds an IPv6 Neighbor Advertisement packet.
pub(crate) fn build_neighbor_advertisement(
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
    target: Ipv6Address,
    src_mac: MacAddress,
    solicited: bool,
) -> Option<Vec<u8>> {
    let mut icmp_payload = vec![0u8; NDP_BASE_PAYLOAD_LEN];
    icmp_payload[..NDP_TARGET_LEN].copy_from_slice(&target.octets());
    write_lla_option(
        &mut icmp_payload[NDP_TARGET_LEN..],
        NDP_OPTION_TARGET_LLA,
        src_mac,
    )?;

    let icmp_type = Icmpv6Type::NeighborAdvertisement(NeighborAdvertisementHeader {
        router: false,
        solicited,
        r#override: true,
    });
    let icmp_header = Icmpv6Header::with_checksum(
        icmp_type,
        src_addr.octets(),
        dst_addr.octets(),
        &icmp_payload,
    )
    .ok()?;

    assemble_ndp_packet(src_addr, dst_addr, &icmp_header, &icmp_payload)
}

/// Parses an inbound ICMPv6 NDP message after hop-limit and checksum
/// validation and returns the learning/action result.
pub(crate) fn handle_ndp_input(
    header: Ipv6Header,
    packet: &[u8],
    local_addrs: &[Ipv6Cidr],
) -> Option<NdpInput> {
    if header.hop_limit() != NDP_HOP_LIMIT {
        return None;
    }
    let icmp = ipv6::payload(packet, &header)?;
    if !super::icmpv6::has_valid_checksum(header.src_addr(), header.dst_addr(), icmp) {
        return None;
    }
    let slice = Icmpv6Slice::from_slice(icmp).ok()?;
    match slice.icmp_type() {
        Icmpv6Type::NeighborSolicitation => {
            parse_neighbor_solicitation(header, slice.payload(), local_addrs)
        }
        Icmpv6Type::NeighborAdvertisement(_) => parse_neighbor_advertisement(slice.payload()),
        _ => None,
    }
}

fn parse_neighbor_solicitation(
    header: Ipv6Header,
    payload: &[u8],
    local_addrs: &[Ipv6Cidr],
) -> Option<NdpInput> {
    if payload.len() < NDP_TARGET_LEN {
        return None;
    }
    let target = Ipv6Address::from_octets(payload[..NDP_TARGET_LEN].try_into().ok()?);
    let learn = if header.src_addr().is_unspecified() {
        None
    } else {
        find_lla_option(&payload[NDP_TARGET_LEN..], NDP_OPTION_SOURCE_LLA)
            .map(|mac| (header.src_addr(), mac))
    };
    let action = local_addrs
        .iter()
        .any(|cidr| cidr.address() == target)
        .then(|| {
            let dst = if header.src_addr().is_unspecified() {
                Ipv6Address::ALL_NODES_LINK_LOCAL
            } else {
                header.src_addr()
            };
            NdpAction::ReplyAdvertisement {
                target,
                dst,
                solicited: !header.src_addr().is_unspecified(),
            }
        });
    Some(NdpInput { learn, action })
}

fn parse_neighbor_advertisement(payload: &[u8]) -> Option<NdpInput> {
    if payload.len() < NDP_TARGET_LEN {
        return None;
    }
    let target = Ipv6Address::from_octets(payload[..NDP_TARGET_LEN].try_into().ok()?);
    let mac = find_lla_option(&payload[NDP_TARGET_LEN..], NDP_OPTION_TARGET_LLA)?;
    Some(NdpInput {
        learn: None,
        action: Some(NdpAction::LearnNeighbor { addr: target, mac }),
    })
}

fn assemble_ndp_packet(
    src_addr: Ipv6Address,
    dst_addr: Ipv6Address,
    icmp_header: &Icmpv6Header,
    icmp_payload: &[u8],
) -> Option<Vec<u8>> {
    let icmp_hdr = icmp_header.to_bytes();
    let icmp_len = icmp_hdr.len() + icmp_payload.len();
    let mut packet = vec![0u8; IPV6_HEADER_LEN + icmp_len];
    ipv6::write_ipv6_packet_header(
        &mut packet,
        src_addr,
        dst_addr,
        PROTOCOL_ICMPV6,
        NDP_HOP_LIMIT,
    )?;
    packet[IPV6_HEADER_LEN..IPV6_HEADER_LEN + icmp_hdr.len()].copy_from_slice(&icmp_hdr);
    packet[IPV6_HEADER_LEN + icmp_hdr.len()..].copy_from_slice(icmp_payload);
    Some(packet)
}

fn write_lla_option(buf: &mut [u8], option_type: u8, mac: MacAddress) -> Option<()> {
    if buf.len() < NDP_LLA_OPTION_LEN {
        return None;
    }
    buf[0] = option_type;
    buf[1] = 1; // length in units of 8 octets
    buf[2..8].copy_from_slice(&mac.bytes());
    Some(())
}

fn find_lla_option(options: &[u8], option_type: u8) -> Option<MacAddress> {
    let mut offset = 0;
    while offset + 2 <= options.len() {
        let ty = options[offset];
        let len_units = options[offset + 1] as usize;
        if len_units == 0 {
            return None;
        }
        let len = len_units * 8;
        if offset + len > options.len() {
            return None;
        }
        if ty == option_type && len >= NDP_LLA_OPTION_LEN {
            let mut mac = [0u8; 6];
            mac.copy_from_slice(&options[offset + 2..offset + 8]);
            return Some(MacAddress(mac));
        }
        offset += len;
    }
    None
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_build_and_parse_neighbor_solicitation() {
        let src = Ipv6Address::link_local_from_mac([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        let target = Ipv6Address::link_local_from_mac([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
        let mac = MacAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);

        let packet = build_neighbor_solicitation(src, target, mac).unwrap();
        let header = Ipv6Header::parse_input(&packet).unwrap();
        assert_eq!(header.dst_addr(), target.solicited_node_multicast());
        assert_eq!(header.hop_limit(), NDP_HOP_LIMIT);

        let input = handle_ndp_input(header, &packet, &[Ipv6Cidr::new(target, 64)]).unwrap();
        assert_eq!(input.learn, Some((src, mac)));
        assert_eq!(
            input.action,
            Some(NdpAction::ReplyAdvertisement {
                target,
                dst: src,
                solicited: true,
            })
        );
    }

    #[def_test]
    fn test_build_and_parse_neighbor_advertisement() {
        let src = Ipv6Address::link_local_from_mac([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
        let dst = Ipv6Address::link_local_from_mac([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        let mac = MacAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);

        let packet = build_neighbor_advertisement(src, dst, src, mac, true).unwrap();
        let header = Ipv6Header::parse_input(&packet).unwrap();
        let input = handle_ndp_input(header, &packet, &[]).unwrap();
        assert_eq!(
            input.action,
            Some(NdpAction::LearnNeighbor { addr: src, mac })
        );
    }
}
