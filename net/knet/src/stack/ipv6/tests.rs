// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};

use unittest::def_test;

use super::{
    packet::{
        IPV6_EXT_FRAGMENT, IPV6_EXT_HOP_BY_HOP, IPV6_HEADER_LEN, Ipv6Error, Ipv6Header,
        PROTOCOL_ICMPV6, PROTOCOL_TCP, PROTOCOL_UDP, build_ipv6_packet,
    },
    payload, process_ipv6_input,
};
use crate::{
    buf::{PacketBuf, PacketOwner},
    ip::{Ipv6Address, Ipv6Cidr},
};

fn sample_addrs() -> (Ipv6Address, Ipv6Address) {
    (
        Ipv6Address::from_octets([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
        Ipv6Address::from_octets([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]),
    )
}

#[def_test]
fn test_validate_input_packet_accepts_plain_udp() {
    let (src, dst) = sample_addrs();
    let bytes = build_ipv6_packet(src, dst, PROTOCOL_UDP, 64, &[0; 8]).unwrap();
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);

    let header = Ipv6Header::validate_input_packet(&mut packet).unwrap();

    assert_eq!(header.next_header(), PROTOCOL_UDP);
    assert_eq!(header.header_len(), IPV6_HEADER_LEN);
    assert_eq!(header.total_len(), IPV6_HEADER_LEN + 8);
    assert_eq!(packet.transport_offset(), Some(IPV6_HEADER_LEN));
}

#[def_test]
fn test_validate_input_packet_skips_hop_by_hop() {
    let (src, dst) = sample_addrs();
    let mut payload = vec![0u8; 8 + 8];
    // Hop-by-Hop: next=UDP, len=0 (8 bytes)
    payload[0] = PROTOCOL_UDP;
    payload[1] = 0;
    let bytes = build_ipv6_packet(src, dst, IPV6_EXT_HOP_BY_HOP, 64, &payload).unwrap();
    // Fix payload_length already set by builder.
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);

    let header = Ipv6Header::validate_input_packet(&mut packet).unwrap();
    assert_eq!(header.next_header(), PROTOCOL_UDP);
    assert_eq!(header.transport_offset(), IPV6_HEADER_LEN + 8);
}

#[def_test]
fn test_validate_input_packet_rejects_fragment() {
    let (src, dst) = sample_addrs();
    let mut payload = vec![0u8; 8];
    payload[0] = PROTOCOL_UDP;
    let bytes = build_ipv6_packet(src, dst, IPV6_EXT_FRAGMENT, 64, &payload).unwrap();
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);

    assert_eq!(
        Ipv6Header::validate_input_packet(&mut packet),
        Err(Ipv6Error::Fragmented)
    );
}

#[def_test]
fn test_input_emits_parameter_problem_for_unknown_next_header() {
    let (src, dst) = sample_addrs();
    let bytes = build_ipv6_packet(src, dst, 132, 64, &[0; 8]).unwrap();
    let packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let mut control_packets = Vec::new();
    let mut actions = Vec::new();

    let result = process_ipv6_input(packet, &local_addrs, &mut control_packets, &mut actions);

    assert!(result.is_none());
    assert!(actions.is_empty());
    let error = control_packets.pop().expect("parameter problem");
    let error_header = Ipv6Header::parse_input(&error).unwrap();
    assert_eq!(error_header.src_addr(), dst);
    assert_eq!(error_header.dst_addr(), src);
    let icmp = payload(&error, &error_header).unwrap();
    assert_eq!(icmp[0], etherparse::icmpv6::TYPE_PARAMETER_PROBLEM);
    assert_eq!(
        icmp[1],
        etherparse::icmpv6::CODE_PARAM_PROBLEM_UNRECOG_NEXT_HEADER
    );
    // Base header next-header field at offset 6.
    assert_eq!(u32::from_be_bytes([icmp[4], icmp[5], icmp[6], icmp[7]]), 6);
}

#[def_test]
fn test_input_parameter_problem_pointer_tracks_extension_header() {
    let (src, dst) = sample_addrs();
    let mut payload_bytes = vec![0u8; 8 + 8];
    // Hop-by-Hop names an unrecognized protocol.
    payload_bytes[0] = 132;
    payload_bytes[1] = 0;
    let bytes = build_ipv6_packet(src, dst, IPV6_EXT_HOP_BY_HOP, 64, &payload_bytes).unwrap();
    let packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let mut control_packets = Vec::new();
    let mut actions = Vec::new();

    let result = process_ipv6_input(packet, &local_addrs, &mut control_packets, &mut actions);

    assert!(result.is_none());
    let error = control_packets.pop().expect("parameter problem");
    let error_header = Ipv6Header::parse_input(&error).unwrap();
    let icmp = payload(&error, &error_header).unwrap();
    // Pointer names the Hop-by-Hop next-header field at offset 40.
    assert_eq!(u32::from_be_bytes([icmp[4], icmp[5], icmp[6], icmp[7]]), 40);
}

#[def_test]
fn test_input_passes_through_tcp_and_suppresses_multicast_parameter_problem() {
    let (src, dst) = sample_addrs();
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let mut control_packets = Vec::new();
    let mut actions = Vec::new();

    // TCP passes through to the smoltcp ingress path.
    let tcp = build_ipv6_packet(src, dst, PROTOCOL_TCP, 64, &[0; 20]).unwrap();
    let packet = PacketBuf::from_ip_packet_vec(1, tcp, PacketOwner::DeviceRx);
    assert!(process_ipv6_input(packet, &local_addrs, &mut control_packets, &mut actions).is_some());

    // Multicast destination must not get a Parameter Problem (RFC 4443 §2.4(a)).
    let multicast =
        Ipv6Address::from_octets([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let unknown = build_ipv6_packet(src, multicast, 132, 64, &[0; 8]).unwrap();
    let packet = PacketBuf::from_ip_packet_vec(1, unknown, PacketOwner::DeviceRx);
    assert!(process_ipv6_input(packet, &[], &mut control_packets, &mut actions).is_none());
    assert!(control_packets.is_empty());
}

#[def_test]
fn test_prepare_output_packet_repairs_payload_len() {
    let (src, dst) = sample_addrs();
    let mut bytes = build_ipv6_packet(src, dst, PROTOCOL_ICMPV6, 64, &[1, 2, 3, 4]).unwrap();
    bytes[4] = 0;
    bytes[5] = 0;
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceTx);

    Ipv6Header::prepare_output_packet(&mut packet).unwrap();
    let header = Ipv6Header::parse_input(packet.network_packet().unwrap()).unwrap();
    assert_eq!(header.next_header(), PROTOCOL_ICMPV6);
    assert_eq!(header.total_len(), IPV6_HEADER_LEN + 4);
}
