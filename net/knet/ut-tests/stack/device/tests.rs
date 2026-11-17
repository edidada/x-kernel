// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::num::NonZeroU16;

use smoltcp::{
    phy::{Device, PacketMeta, RxToken as _, TxToken as _},
    wire::{IpAddress, TcpPacket},
};
use unittest::def_test;

use super::*;
use crate::ipv4::Ipv4Header;

#[def_test]
fn tx_backpressure_preserves_rx_until_a_response_slot_is_available() {
    let mut rx = VecDeque::from([PacketBuf::from_ip_packet_vec(
        1,
        vec![0x45, 1],
        PacketOwner::DeviceRx,
    )]);
    let mut tx = DataTxQueue::with_capacity(1);
    tx.try_push(PacketBuf::from_ip_packet_vec(
        0,
        vec![0],
        PacketOwner::Ipv4Stack,
    ))
    .unwrap();
    let now = Instant::from_millis(0);
    {
        let mut device = StackDevice::new(&mut rx, &mut tx, 1500);
        assert!(device.receive(now).is_none());
        assert!(device.transmit(now).is_none());
    }
    assert_eq!(rx.len(), 1);
    tx.pop_front().unwrap();
    {
        let mut device = StackDevice::new(&mut rx, &mut tx, 1500);
        let (received, response) = device.receive(now).unwrap();
        received.consume(|bytes| assert_eq!(bytes, &[0x45, 1]));
        response.consume(2, |bytes| bytes.copy_from_slice(&[0x45, 2]));
    }
    assert!(rx.is_empty());
    assert_eq!(
        tx.pop_front().unwrap().network_packet(),
        Some(&[0x45, 2][..])
    );
}

#[def_test]
fn dropping_a_tx_token_does_not_consume_queue_capacity() {
    let mut rx = VecDeque::new();
    let mut tx = DataTxQueue::with_capacity(1);
    {
        let mut device = StackDevice::new(&mut rx, &mut tx, 9000);
        let caps = device.capabilities();
        assert_eq!(caps.medium, Medium::Ip);
        assert_eq!(caps.max_transmission_unit, 9000);
        assert_eq!(caps.max_burst_size, Some(SOCKET_BUFFER_SIZE));
        assert!(caps.checksum.tcp.rx() && !caps.checksum.tcp.tx());
        assert_eq!(caps.segmentation.tcpv4, Some(TSO_MAX_IP_PACKET_LEN));
        assert!(device.transmit(Instant::from_millis(0)).is_some());
    }
    assert_eq!(tx.available(), 1);
}

#[def_test]
fn tx_token_segments_tso_packets_and_fills_tcp_checksums() {
    let mut rx = VecDeque::new();
    let mut tx = DataTxQueue::with_capacity(1);
    let now = Instant::from_millis(0);

    {
        let mut device = StackDevice::new(&mut rx, &mut tx, 1500);
        let mut tok = device.transmit(now).unwrap();
        let mut meta = PacketMeta::default();
        meta.segmentation_offload_size = Some(NonZeroU16::new(1000).unwrap());
        tok.set_meta(meta);
        tok.consume(20 + 20 + 2500, |bytes| {
            bytes.fill(0);
            bytes[0] = 0x45;
            bytes[8] = 64;
            bytes[9] = 6;
            bytes[12..16].copy_from_slice(&[10, 0, 2, 15]);
            bytes[16..20].copy_from_slice(&[10, 0, 2, 2]);
            bytes[20..22].copy_from_slice(&50000u16.to_be_bytes());
            bytes[22..24].copy_from_slice(&5201u16.to_be_bytes());
            bytes[24..28].copy_from_slice(&100u32.to_be_bytes());
            bytes[32] = 0x50;
            // CWR(0x80) | ACK(0x10) | PSH(0x08) | FIN(0x01)
            bytes[33] = 0x99;
            for (idx, b) in bytes[40..].iter_mut().enumerate() {
                *b = (idx & 0xff) as u8;
            }
        });
    }

    let src = IpAddress::v4(10, 0, 2, 15);
    let dst = IpAddress::v4(10, 0, 2, 2);
    let expected_lens = [1000usize, 1000, 500];
    let mut seq = 100i32;

    for (idx, &payload_len) in expected_lens.iter().enumerate() {
        assert!(tx.is_full());
        assert_eq!(tx.available(), 0);
        let pkt = tx.pop_front().unwrap();
        let bytes = pkt.network_packet().unwrap();
        assert_eq!(bytes.len(), 40 + payload_len);
        assert!(Ipv4Header::parse_input(bytes).is_ok());

        let tcp = TcpPacket::new_checked(&bytes[20..]).unwrap();
        assert_eq!(tcp.seq_number().0, seq);
        assert!(tcp.verify_checksum(&src, &dst));
        assert_eq!(tcp.cwr(), idx == 0);
        assert_eq!(tcp.psh(), idx == 2);
        assert_eq!(tcp.fin(), idx == 2);
        seq += payload_len as i32;
    }
    assert!(tx.pop_front().is_none());
    assert_eq!(tx.available(), 1);
}
