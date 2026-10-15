// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use unittest::def_test;

use super::*;
use crate::buf::PacketOwner;

fn ipv4_packet(protocol: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![0u8; IPV4_MIN_HEADER_LEN + payload.len()];
    write_ipv4_header(
        &mut packet[..IPV4_MIN_HEADER_LEN],
        Ipv4Address::new(192, 0, 2, 1),
        Ipv4Address::new(192, 0, 2, 2),
        protocol,
        64,
        payload.len(),
        Ipv4HeaderFragment::NONE,
    )
    .unwrap();
    packet[IPV4_MIN_HEADER_LEN..].copy_from_slice(payload);
    packet
}

fn dont_fragment_ipv4_packet(protocol: u8, payload: &[u8]) -> Vec<u8> {
    build_ipv4_packet_dont_fragment(
        Ipv4Address::new(192, 0, 2, 1),
        Ipv4Address::new(192, 0, 2, 2),
        protocol,
        64,
        payload,
    )
    .unwrap()
}

#[def_test]
fn test_validate_input_packet_accepts_valid_checksum() {
    let mut bytes = ipv4_packet(PROTOCOL_UDP, &[1, 2]);
    bytes.extend_from_slice(&[0; 16]);
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);

    let view = Ipv4PacketView::validate_input_packet(&mut packet).unwrap();
    let header = view.header();

    assert_eq!(view.payload(), &[1, 2]);
    assert_eq!(view.packet_len(), IPV4_MIN_HEADER_LEN + 2);
    assert_eq!(view.packet().len(), header.total_len());
    assert_eq!(header.protocol(), PROTOCOL_UDP);
    assert_eq!(header.header_len(), IPV4_MIN_HEADER_LEN);
    assert_eq!(header.total_len(), IPV4_MIN_HEADER_LEN + 2);
    assert_eq!(packet.transport_offset(), Some(IPV4_MIN_HEADER_LEN));
    assert_eq!(packet.checksum_state(), ChecksumState::Verified);
}

#[def_test]
fn test_packet_view_enforces_payload_bounds() {
    let bytes = ipv4_packet(PROTOCOL_UDP, &[1, 2]);
    let header = Ipv4Header::parse_input(&bytes).unwrap();

    for total_len in [header.header_len() - 1, bytes.len() + 1] {
        let invalid_header = Ipv4Header {
            total_len,
            ..header
        };
        assert_eq!(
            Ipv4PacketView::from_header(&bytes, invalid_header),
            Err(Ipv4Error::Malformed)
        );
    }

    let empty_payload_header = Ipv4Header {
        total_len: header.header_len(),
        ..header
    };
    let view = Ipv4PacketView::from_header(&bytes, empty_payload_header).unwrap();
    assert!(view.payload().is_empty());
}

#[def_test]
fn test_verified_checksum_view_preserves_payload_bounds() {
    let mut bytes = ipv4_packet(PROTOCOL_TCP, &[1, 2]);
    bytes.extend_from_slice(&[0; 16]);
    let view = Ipv4PacketView::parse_input_with_verified_checksum(&bytes).unwrap();
    assert_eq!(view, Ipv4PacketView::parse_input(&bytes).unwrap());
    assert_eq!(view.payload(), &[1, 2]);

    for total_len in [IPV4_MIN_HEADER_LEN - 1, bytes.len() + 1] {
        bytes[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
        assert_eq!(
            Ipv4PacketView::parse_input_with_verified_checksum(&bytes),
            Err(Ipv4Error::Malformed)
        );
    }
}

#[def_test]
fn test_validate_input_packet_marks_fragment() {
    let mut bytes = ipv4_packet(PROTOCOL_UDP, &[1, 2]);
    let mut header = Ipv4HeaderSlice::from_slice(&bytes).unwrap().to_header();
    header.more_fragments = true;
    header.header_checksum = header.calc_header_checksum();
    bytes[..IPV4_MIN_HEADER_LEN].copy_from_slice(&header.to_bytes());
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);

    let header = Ipv4PacketView::validate_input_packet(&mut packet)
        .unwrap()
        .header();

    assert!(header.is_fragmented());
}

#[def_test]
fn test_validate_input_packet_rejects_bad_checksum() {
    let mut bytes = ipv4_packet(PROTOCOL_TCP, &[]);
    bytes[10] ^= 0xff;
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);

    assert_eq!(
        Ipv4PacketView::validate_input_packet(&mut packet),
        Err(Ipv4Error::BadChecksum)
    );
}

#[def_test]
fn test_prepare_output_packet_repairs_checksum() {
    let mut bytes = ipv4_packet(PROTOCOL_ICMP, &[1, 2, 3]);
    bytes[2..4].fill(0);
    bytes[10..12].fill(0);
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceTx);

    Ipv4Header::prepare_output_packet(&mut packet).unwrap();

    let view = Ipv4PacketView::parse_input(packet.network_packet().unwrap()).unwrap();
    assert_eq!(view.header().protocol(), PROTOCOL_ICMP);
    assert_eq!(view.header().total_len(), IPV4_MIN_HEADER_LEN + 3);
    assert_eq!(view.payload(), &[1, 2, 3]);
    assert_eq!(packet.transport_offset(), Some(IPV4_MIN_HEADER_LEN));
    assert_eq!(packet.checksum_state(), ChecksumState::Verified);
}

#[def_test]
fn test_output_view_accepts_unfinalized_header() {
    let mut bytes = ipv4_packet(PROTOCOL_UDP, &[1, 2, 3]);
    bytes[2..4].fill(0);
    bytes[10..12].fill(0);

    let view = Ipv4PacketView::parse_output(&bytes).unwrap();

    assert_eq!(view.packet(), bytes.as_slice());
    assert_eq!(view.packet_len(), IPV4_MIN_HEADER_LEN + 3);
    assert_eq!(view.header().total_len(), view.packet_len());
    assert_eq!(view.payload(), &[1, 2, 3]);
    assert_eq!(
        Ipv4PacketView::parse_input(&bytes),
        Err(Ipv4Error::Malformed)
    );
}

#[def_test]
fn test_fragment_output_packet_splits_on_aligned_payload() {
    let original_payload: Vec<u8> = (0..40).collect();
    let mut packet = ipv4_packet(PROTOCOL_UDP, &original_payload);
    packet[2..4].fill(0);
    packet[10..12].fill(0);

    let fragments = fragment_output_packet(
        Ipv4PacketView::parse_output(&packet).unwrap(),
        IPV4_MIN_HEADER_LEN + 18,
        0x1234,
    )
    .unwrap();

    assert_eq!(fragments.len(), 3);
    let mut reassembled = Vec::new();
    for (idx, fragment) in fragments.iter().enumerate() {
        let view = Ipv4PacketView::parse_input(fragment).unwrap();
        let header = view.header();
        assert_eq!(header.identification(), 0x1234);
        assert_eq!(header.fragment_offset(), idx * 16);
        assert_eq!(header.more_fragments(), idx != 2);
        assert!(!header.dont_fragment());
        reassembled.extend_from_slice(view.payload());
    }
    assert_eq!(reassembled, original_payload);
}

#[def_test]
fn test_fragment_output_packet_rejects_dont_fragment() {
    let packet = dont_fragment_ipv4_packet(PROTOCOL_UDP, &[0u8; 32]);

    assert_eq!(
        fragment_output_packet(
            Ipv4PacketView::parse_output(&packet).unwrap(),
            IPV4_MIN_HEADER_LEN + 16,
            0x1234,
        ),
        Err(Ipv4FragmentError::DontFragment)
    );
}

#[def_test]
fn test_build_icmpv4_protocol_unreachable_packet() {
    let offending = ipv4_packet(99, &[1, 2, 3, 4]);
    let header = Ipv4Header::parse_input(&offending).unwrap();

    let error = build_icmpv4_error_packet(
        Icmpv4Error::ProtocolUnreachable,
        PacketType::Host,
        header,
        &offending,
    )
    .expect("ICMP error should be emitted");

    let error_header = Ipv4Header::parse_input(&error).unwrap();
    assert_eq!(error_header.protocol(), PROTOCOL_ICMP);
    assert_eq!(error_header.src_addr(), header.dst_addr());
    assert_eq!(error_header.dst_addr(), header.src_addr());
    assert_eq!(
        error[IPV4_MIN_HEADER_LEN],
        etherparse::icmpv4::TYPE_DEST_UNREACH
    );
    assert_eq!(
        error[IPV4_MIN_HEADER_LEN + 1],
        etherparse::icmpv4::CODE_DST_UNREACH_PROTOCOL
    );
}
