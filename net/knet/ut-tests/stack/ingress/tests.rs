// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use unittest::def_test;

use super::*;
use crate::buf::PacketOwner;

#[def_test]
fn accepted_ipv4_packets_reject_bad_checksums_and_preserve_validated_bytes() {
    let local = Ipv4Address::new(192, 0, 2, 2);
    let mut ingress = IngressProcessor::new();
    ingress.update_local_ipv4_addrs(&[Ipv4Cidr::new(local, 24).unwrap()]);
    let bytes = ipv4::build_ipv4_packet(
        Ipv4Address::new(192, 0, 2, 1),
        local,
        ipv4::PROTOCOL_TCP,
        64,
        &[0; 20],
    )
    .unwrap();
    let mut padded = bytes.clone();
    padded.extend_from_slice(&[0; 16]);
    let mut alias = PacketBuf::from_ip_packet_vec(1, padded, PacketOwner::DeviceRx);
    let mut invalid = bytes.clone();
    invalid[8] ^= 1;
    let mut packets = vec![
        PacketBuf::from_ip_packet_vec(1, invalid, PacketOwner::DeviceRx),
        alias.clone(),
    ];
    let mut accepted = Vec::new();
    let mut control = Vec::new();

    ingress.handle_rx_packets(
        MonotonicInstant::ORIGIN,
        &mut packets,
        &mut accepted,
        &mut control,
    );

    assert!(packets.is_empty());
    assert!(control.is_empty());
    assert_eq!(accepted.len(), 1);
    // Another owner must not be able to invalidate a retained batch.
    alias.network_packet_mut().unwrap()[8] ^= 1;
    let retained = accepted.pop().unwrap().into_packet();
    assert_eq!(retained.network_packet().unwrap(), bytes.as_slice());
    assert!(Ipv4PacketView::parse_input(retained.network_packet().unwrap()).is_ok());
}
