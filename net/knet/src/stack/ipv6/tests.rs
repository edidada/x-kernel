// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};

use etherparse::{IpFragOffset, IpNumber, Ipv6FragmentHeader};
use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::def_test;

use super::{
    Ipv6Reassembler,
    output::Ipv6Action,
    packet::{
        IPV6_EXT_FRAGMENT, IPV6_EXT_HOP_BY_HOP, IPV6_EXT_ROUTING, IPV6_HEADER_LEN, Ipv6Error,
        Ipv6Header, PROTOCOL_ICMPV6, PROTOCOL_TCP, PROTOCOL_UDP, build_ipv6_packet,
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

fn process(
    packet: PacketBuf,
    local_addrs: &[Ipv6Cidr],
) -> (Option<PacketBuf>, Vec<Vec<u8>>, Vec<Ipv6Action>) {
    let mut reassembler = Ipv6Reassembler::new();
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
    let mut control_packets = Vec::new();
    let mut actions = Vec::new();
    let result = process_ipv6_input(
        now,
        packet,
        local_addrs,
        &mut reassembler,
        &mut control_packets,
        &mut actions,
    );
    (result, control_packets, actions)
}

fn frag_hdr(next_header: u8, offset: usize, more_fragments: bool) -> [u8; 8] {
    let offset_units = u16::try_from(offset / 8).unwrap();
    Ipv6FragmentHeader::new(
        IpNumber(next_header),
        IpFragOffset::try_new(offset_units).unwrap(),
        more_fragments,
        0x1234_5678,
    )
    .to_bytes()
}

fn fragment_packet(next_header: u8, offset: usize, more_fragments: bool, data: &[u8]) -> PacketBuf {
    let (src, dst) = sample_addrs();
    let mut body = frag_hdr(next_header, offset, more_fragments).to_vec();
    body.extend_from_slice(data);
    let bytes = build_ipv6_packet(src, dst, IPV6_EXT_FRAGMENT, 64, &body).unwrap();
    PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx)
}

fn udp_datagram(payload: &[u8]) -> Vec<u8> {
    let (src, dst) = sample_addrs();
    let mut header = etherparse::UdpHeader::without_ipv4_checksum(1234, 53, payload.len()).unwrap();
    let checksum = header
        .calc_checksum_ipv6_raw(src.octets(), dst.octets(), payload)
        .unwrap();
    header.checksum = if checksum == 0 { 0xffff } else { checksum };
    let mut bytes = header.to_bytes().to_vec();
    bytes.extend_from_slice(payload);
    bytes
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

    let (result, control_packets, actions) = process(packet, &local_addrs);

    assert!(result.is_none());
    assert!(actions.is_empty());
    let error = control_packets
        .into_iter()
        .next()
        .expect("parameter problem");
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

    let (result, control_packets, _) = process(packet, &local_addrs);

    assert!(result.is_none());
    let error = control_packets
        .into_iter()
        .next()
        .expect("parameter problem");
    let error_header = Ipv6Header::parse_input(&error).unwrap();
    let icmp = payload(&error, &error_header).unwrap();
    // Pointer names the Hop-by-Hop next-header field at offset 40.
    assert_eq!(u32::from_be_bytes([icmp[4], icmp[5], icmp[6], icmp[7]]), 40);
}

#[def_test]
fn test_input_passes_through_tcp_and_suppresses_multicast_parameter_problem() {
    let (src, dst) = sample_addrs();
    let local_addrs = [Ipv6Cidr::new(dst, 128)];

    // TCP passes through to the smoltcp ingress path.
    let tcp = build_ipv6_packet(src, dst, PROTOCOL_TCP, 64, &[0; 20]).unwrap();
    let packet = PacketBuf::from_ip_packet_vec(1, tcp, PacketOwner::DeviceRx);
    let (result, control_packets, _) = process(packet, &local_addrs);
    assert!(result.is_some());
    assert!(control_packets.is_empty());

    // Multicast destination must not get a Parameter Problem (RFC 4443 §2.4(a)).
    let multicast =
        Ipv6Address::from_octets([0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let unknown = build_ipv6_packet(src, multicast, 132, 64, &[0; 8]).unwrap();
    let packet = PacketBuf::from_ip_packet_vec(1, unknown, PacketOwner::DeviceRx);
    let (result, control_packets, _) = process(packet, &[]);
    assert!(result.is_none());
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

#[def_test]
fn test_input_reassembles_tcp_fragments_for_smoltcp() {
    let (_, dst) = sample_addrs();
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let mut reassembler = Ipv6Reassembler::new();
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
    let mut control_packets = Vec::new();
    let mut actions = Vec::new();

    // First fragment must include the 20-byte TCP header (RFC 8200 §4.5).
    let first = fragment_packet(PROTOCOL_TCP, 0, true, &[0; 24]);
    assert!(
        process_ipv6_input(
            now,
            first,
            &local_addrs,
            &mut reassembler,
            &mut control_packets,
            &mut actions,
        )
        .is_none()
    );

    let second = fragment_packet(PROTOCOL_TCP, 24, false, &[0; 8]);
    let packet = process_ipv6_input(
        now,
        second,
        &local_addrs,
        &mut reassembler,
        &mut control_packets,
        &mut actions,
    )
    .expect("reassembled TCP");
    let ip_packet = packet.network_packet().unwrap();
    let header = Ipv6Header::parse_input(ip_packet).unwrap();
    assert_eq!(header.next_header(), PROTOCOL_TCP);
    assert_eq!(payload(ip_packet, &header).unwrap().len(), 32);
    assert!(control_packets.is_empty());
}

#[def_test]
fn test_input_atomic_fragment_is_processed_as_complete_tcp() {
    let (_, dst) = sample_addrs();
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let packet = fragment_packet(PROTOCOL_TCP, 0, false, &[0; 20]);
    let (result, control_packets, _) = process(packet, &local_addrs);
    let packet = result.expect("atomic TCP");
    let ip_packet = packet.network_packet().unwrap();
    let header = Ipv6Header::parse_input(ip_packet).unwrap();
    assert_eq!(header.next_header(), PROTOCOL_TCP);
    assert_eq!(payload(ip_packet, &header).unwrap().len(), 20);
    assert!(control_packets.is_empty());
}

#[def_test]
fn test_input_reassembled_udp_without_socket_emits_port_unreachable() {
    let (_, dst) = sample_addrs();
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let mut reassembler = Ipv6Reassembler::new();
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
    let mut control_packets = Vec::new();
    let mut actions = Vec::new();

    let datagram = udp_datagram(&[1, 2, 3, 4]);
    let first = fragment_packet(PROTOCOL_UDP, 0, true, &datagram[..8]);
    assert!(
        process_ipv6_input(
            now,
            first,
            &local_addrs,
            &mut reassembler,
            &mut control_packets,
            &mut actions,
        )
        .is_none()
    );
    assert!(control_packets.is_empty());

    let second = fragment_packet(PROTOCOL_UDP, 8, false, &datagram[8..]);
    assert!(
        process_ipv6_input(
            now,
            second,
            &local_addrs,
            &mut reassembler,
            &mut control_packets,
            &mut actions,
        )
        .is_none()
    );
    let error = control_packets.pop().expect("port unreachable");
    let error_header = Ipv6Header::parse_input(&error).unwrap();
    let icmp = payload(&error, &error_header).unwrap();
    assert_eq!(icmp[0], etherparse::icmpv6::TYPE_DST_UNREACH);
    assert_eq!(icmp[1], etherparse::icmpv6::CODE_DST_UNREACH_PORT);
}

#[def_test]
fn test_input_truncated_first_fragment_emits_incomplete_header_chain() {
    let (_, dst) = sample_addrs();
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let packet = fragment_packet(PROTOCOL_TCP, 0, true, &[0; 8]);
    let (result, control_packets, _) = process(packet, &local_addrs);
    assert!(result.is_none());
    let error = control_packets
        .into_iter()
        .next()
        .expect("parameter problem");
    let error_header = Ipv6Header::parse_input(&error).unwrap();
    let icmp = payload(&error, &error_header).unwrap();
    assert_eq!(icmp[0], etherparse::icmpv6::TYPE_PARAMETER_PROBLEM);
    assert_eq!(
        icmp[1],
        etherparse::icmpv6::CODE_PARAM_PROBLEM_IPV6_FIRST_FRAG_INCOMP_HEADER_CHAIN
    );
    assert_eq!(u32::from_be_bytes([icmp[4], icmp[5], icmp[6], icmp[7]]), 0);
}

#[def_test]
fn test_input_unaligned_more_fragment_emits_erroneous_header_field() {
    let (_, dst) = sample_addrs();
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let packet = fragment_packet(PROTOCOL_UDP, 0, true, &[0; 9]);
    let (result, control_packets, _) = process(packet, &local_addrs);
    assert!(result.is_none());
    let error = control_packets
        .into_iter()
        .next()
        .expect("parameter problem");
    let error_header = Ipv6Header::parse_input(&error).unwrap();
    let icmp = payload(&error, &error_header).unwrap();
    assert_eq!(icmp[0], etherparse::icmpv6::TYPE_PARAMETER_PROBLEM);
    assert_eq!(
        icmp[1],
        etherparse::icmpv6::CODE_PARAM_PROBLEM_ERR_HEADER_FIELD
    );
    assert_eq!(u32::from_be_bytes([icmp[4], icmp[5], icmp[6], icmp[7]]), 4);
}

#[def_test]
fn test_input_timeout_with_first_fragment_emits_time_exceeded() {
    let (_, dst) = sample_addrs();
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let mut reassembler = Ipv6Reassembler::new();
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
    let mut control_packets = Vec::new();
    let mut actions = Vec::new();

    let first = fragment_packet(PROTOCOL_UDP, 0, true, &[0; 8]);
    assert!(
        process_ipv6_input(
            now,
            first,
            &local_addrs,
            &mut reassembler,
            &mut control_packets,
            &mut actions,
        )
        .is_none()
    );

    for expired in reassembler.remove_expired(now + TimeSpan::from_secs(60)) {
        crate::ipv6::queue_icmpv6_error(
            &mut control_packets,
            crate::icmpv6::Icmpv6Error::FragmentReassemblyTimeout,
            expired.packet_type,
            expired.header,
            &expired.packet,
        );
    }
    let error = control_packets.pop().expect("time exceeded");
    let error_header = Ipv6Header::parse_input(&error).unwrap();
    let icmp = payload(&error, &error_header).unwrap();
    assert_eq!(icmp[0], etherparse::icmpv6::TYPE_TIME_EXCEEDED);
    assert_eq!(
        icmp[1],
        etherparse::icmpv6::CODE_TIME_EXCEEDED_FRAGMENT_REASSEMBLY_TIME_EXCEEDED
    );
}

fn routing_header(next_header: u8) -> [u8; 8] {
    let mut hdr = [0u8; 8];
    hdr[0] = next_header;
    hdr
}

fn nested_fragment_after_routing(upper: &[u8]) -> Vec<u8> {
    let mut payload = routing_header(IPV6_EXT_FRAGMENT).to_vec();
    payload.extend_from_slice(&frag_hdr(PROTOCOL_UDP, 0, false));
    payload.extend_from_slice(upper);
    payload
}

#[def_test]
fn test_atomic_fragment_with_inner_fragment_after_routing_is_dropped() {
    let (_, dst) = sample_addrs();
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let packet = fragment_packet(
        IPV6_EXT_ROUTING,
        0,
        false,
        &nested_fragment_after_routing(&udp_datagram(b"nested")),
    );
    let (result, control_packets, actions) = process(packet, &local_addrs);
    assert!(result.is_none());
    assert!(actions.is_empty());
    assert!(
        control_packets.is_empty(),
        "nested Fragment after Routing must not be delivered as UDP"
    );
}

#[def_test]
fn test_first_fragment_routing_then_fragment_is_not_parameter_problem() {
    let (_, dst) = sample_addrs();
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let mut fragmentable = routing_header(IPV6_EXT_FRAGMENT).to_vec();
    fragmentable.extend_from_slice(&frag_hdr(PROTOCOL_UDP, 0, false));
    let packet = fragment_packet(IPV6_EXT_ROUTING, 0, true, &fragmentable);
    let (result, control_packets, _) = process(packet, &local_addrs);
    assert!(result.is_none());
    assert!(
        control_packets.is_empty(),
        "Linux ipv6frag_thdr_truncated does not send code 3 when skip hits Fragment"
    );
}

#[def_test]
fn test_reassembled_inner_fragment_after_routing_is_dropped() {
    let (_, dst) = sample_addrs();
    let local_addrs = [Ipv6Cidr::new(dst, 128)];
    let mut reassembler = Ipv6Reassembler::new();
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(1));
    let mut control_packets = Vec::new();
    let mut actions = Vec::new();

    let mut first_payload = routing_header(IPV6_EXT_FRAGMENT).to_vec();
    first_payload.extend_from_slice(&frag_hdr(PROTOCOL_UDP, 0, false));
    let first = fragment_packet(IPV6_EXT_ROUTING, 0, true, &first_payload);
    assert!(
        process_ipv6_input(
            now,
            first,
            &local_addrs,
            &mut reassembler,
            &mut control_packets,
            &mut actions,
        )
        .is_none()
    );

    let udp = udp_datagram(b"inner");
    let second = fragment_packet(IPV6_EXT_ROUTING, 16, false, &udp);
    let result = process_ipv6_input(
        now,
        second,
        &local_addrs,
        &mut reassembler,
        &mut control_packets,
        &mut actions,
    );
    assert!(result.is_none());
    assert!(actions.is_empty());
    assert!(
        control_packets.is_empty(),
        "reassembled nested Fragment must be dropped, not delivered"
    );
}
