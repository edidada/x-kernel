// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use etherparse::{IpEcn, IpFragOffset, IpNumber, Ipv4Header as EtherIpv4Header};
use unittest::def_test;

use super::*;

fn wire_header() -> EtherIpv4Header {
    let mut header =
        EtherIpv4Header::new(16, 37, IpNumber::UDP, [192, 0, 2, 1], [192, 0, 2, 2]).unwrap();
    header.options = [1, 1, 0, 0].into();
    header.total_len = (header.header_len() + 16) as u16;
    header.identification = 0x1234;
    header.ecn = IpEcn::try_new(3).unwrap();
    header
}

fn packet(mut header: EtherIpv4Header) -> Vec<u8> {
    header.header_checksum = header.calc_header_checksum();
    let mut bytes = header.to_bytes().to_vec();
    bytes.extend_from_slice(&[0xaa; 16]);
    bytes
}

#[def_test]
fn input_decodes_fields_and_options_length_with_link_padding() {
    let mut bytes = packet(wire_header());
    bytes.extend_from_slice(&[0xbb; 12]);
    let header = Ipv4Header::parse(&bytes).unwrap();
    assert_eq!(header.src_addr(), Ipv4Address::new(192, 0, 2, 1));
    assert_eq!(header.dst_addr(), Ipv4Address::new(192, 0, 2, 2));
    assert_eq!(header.protocol(), 17);
    assert_eq!(header.header_len(), 24);
    assert_eq!(header.total_len(), 40);
    assert_eq!(header.identification(), 0x1234);
    assert_eq!(header.ecn(), 3);
    assert!(!header.is_broadcast_or_multicast());
}

#[def_test]
fn fragment_accessors_derive_byte_offsets_and_status_from_wire_fields() {
    for (dont_fragment, more_fragments, offset_units, is_fragmented) in [
        (true, false, 0, false),
        (false, false, 0, false),
        (false, true, 0, true),
        (false, false, 1, true),
        (false, true, 0x1fff, true),
    ] {
        let mut wire = wire_header();
        wire.dont_fragment = dont_fragment;
        wire.more_fragments = more_fragments;
        wire.fragment_offset = IpFragOffset::try_new(offset_units).unwrap();
        let header = Ipv4Header::parse(&packet(wire)).unwrap();
        assert_eq!(header.dont_fragment(), dont_fragment);
        assert_eq!(header.more_fragments(), more_fragments);
        assert_eq!(header.fragment_offset(), usize::from(offset_units) * 8);
        assert_eq!(header.is_fragmented(), is_fragmented);
    }
}

#[def_test]
fn decoding_requires_header_structure_but_preserves_unfinalized_wire_fields() {
    let mut bytes = packet(wire_header());
    for len in [0, 19, 20, 23] {
        assert_eq!(Ipv4Header::parse(&bytes[..len]), Err(Ipv4Error::Malformed));
    }
    // Only the header needs to be present, even when total_len describes more.
    assert_eq!(Ipv4Header::parse(&bytes[..24]).unwrap().total_len(), 40);
    bytes[2..4].fill(0);
    bytes[10..12].fill(0);
    assert_eq!(Ipv4Header::parse(&bytes).unwrap().total_len(), 0);
    bytes[0] = 0x66;
    assert_eq!(Ipv4Header::parse(&bytes), Err(Ipv4Error::Malformed));
}
