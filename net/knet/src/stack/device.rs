// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Queue-only smoltcp adapter; it has no route or physical-device access.

use alloc::{collections::VecDeque, vec, vec::Vec};
use core::{
    net::{Ipv4Addr, Ipv6Addr},
    num::{NonZeroU16, NonZeroUsize},
};

use smoltcp::{
    phy::{Checksum, DeviceCapabilities, Medium, PacketMeta},
    time::Instant,
    wire::{IpAddress, IpProtocol, TcpPacket, checksum},
};

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::SOCKET_BUFFER_SIZE,
    ipv4::PROTOCOL_TCP,
    runtime::tx_queue::DataTxQueue,
};

const IPV4_MIN_HEADER_LEN: usize = 20;
const IPV6_HEADER_LEN: usize = 40;
const TCP_MIN_HEADER_LEN: usize = 20;
const TCP_FLAG_FIN: u8 = 0x01;
const TCP_FLAG_PSH: u8 = 0x08;
const TCP_FLAG_CWR: u8 = 0x80;

/// Maximum IPv4 packet size advertised for smoltcp TCP Segmentation Offload (65,535 bytes).
const TSO_MAX_IP_PACKET_LEN: NonZeroUsize = NonZeroUsize::MIN.saturating_add(65_534);

/// Borrows queue ownership for one synchronous smoltcp operation.
///
/// The caller serializes both queues for the adapter's entire lifetime. A TX
/// token retains the exclusive queue borrow through its capacity check and
/// enqueue, so another producer cannot consume the selected slot.
pub(crate) struct StackDevice<'a> {
    rx: &'a mut VecDeque<PacketBuf>,
    tx: &'a mut DataTxQueue,
    mtu: usize,
}

impl<'a> StackDevice<'a> {
    pub(crate) fn pending_ip_packet(&self) -> Option<&[u8]> {
        if self.tx.is_full() {
            return None;
        }
        self.rx.front()?.network_packet()
    }

    pub(super) fn new(
        rx: &'a mut VecDeque<PacketBuf>,
        tx: &'a mut DataTxQueue,
        mtu: usize,
    ) -> Self {
        Self { rx, tx, mtu }
    }
}

pub(crate) struct TxToken<'a> {
    queue: &'a mut DataTxQueue,
    gso_size: Option<NonZeroU16>,
}

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn set_meta(&mut self, meta: PacketMeta) {
        self.gso_size = meta.segmentation_offload_size;
    }

    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut ip_packet = vec![0u8; len];
        let result = f(&mut ip_packet);
        if let Some(gso_size) = self.gso_size {
            enqueue_tso_packet(self.queue, ip_packet, usize::from(gso_size.get()));
        } else {
            finalize_tcp_checksum_if_needed(&mut ip_packet);
            assert!(
                self.queue
                    .try_push(PacketBuf::from_ip_packet_vec(
                        0,
                        ip_packet,
                        PacketOwner::Ipv4Stack,
                    ))
                    .is_ok(),
                "smoltcp consumed a TX token without queue capacity"
            );
        }
        result
    }
}

fn parse_ipv4_tcp_bounds(ip_packet: &[u8]) -> Option<(usize, usize)> {
    if ip_packet.len() < IPV4_MIN_HEADER_LEN + TCP_MIN_HEADER_LEN || (ip_packet[0] >> 4) != 4 {
        return None;
    }
    let ihl = usize::from(ip_packet[0] & 0x0f) * 4;
    if ihl < IPV4_MIN_HEADER_LEN
        || ip_packet[9] != PROTOCOL_TCP
        || ip_packet.len() < ihl + TCP_MIN_HEADER_LEN
    {
        return None;
    }
    let tcp_hdr_len = usize::from(ip_packet[ihl + 12] >> 4) * 4;
    if tcp_hdr_len < TCP_MIN_HEADER_LEN || ip_packet.len() < ihl + tcp_hdr_len {
        return None;
    }
    Some((ihl, tcp_hdr_len))
}

fn finalize_tcp_checksum_if_needed(ip_packet: &mut [u8]) {
    match ip_packet.first().map(|byte| byte >> 4) {
        Some(4) => {
            let Some((ihl, _)) = parse_ipv4_tcp_bounds(ip_packet) else {
                return;
            };
            if ip_packet[ihl + 16..ihl + 18] != [0, 0] {
                return;
            }
            let (Ok(src), Ok(dst)) = (
                <[u8; 4]>::try_from(&ip_packet[12..16]),
                <[u8; 4]>::try_from(&ip_packet[16..20]),
            ) else {
                return;
            };
            let src = IpAddress::Ipv4(Ipv4Addr::from(src));
            let dst = IpAddress::Ipv4(Ipv4Addr::from(dst));
            TcpPacket::new_unchecked(&mut ip_packet[ihl..]).fill_checksum(&src, &dst);
        }
        Some(6) => {
            if ip_packet.len() < IPV6_HEADER_LEN + TCP_MIN_HEADER_LEN
                || ip_packet[6] != PROTOCOL_TCP
            {
                return;
            }
            let tcp_hdr_len = usize::from(ip_packet[IPV6_HEADER_LEN + 12] >> 4) * 4;
            if tcp_hdr_len < TCP_MIN_HEADER_LEN
                || ip_packet.len() < IPV6_HEADER_LEN + tcp_hdr_len
                || ip_packet[IPV6_HEADER_LEN + 16..IPV6_HEADER_LEN + 18] != [0, 0]
            {
                return;
            }
            let (Ok(src), Ok(dst)) = (
                <[u8; 16]>::try_from(&ip_packet[8..24]),
                <[u8; 16]>::try_from(&ip_packet[24..40]),
            ) else {
                return;
            };
            let src = IpAddress::Ipv6(Ipv6Addr::from(src));
            let dst = IpAddress::Ipv6(Ipv6Addr::from(dst));
            TcpPacket::new_unchecked(&mut ip_packet[IPV6_HEADER_LEN..]).fill_checksum(&src, &dst);
        }
        _ => {}
    }
}

fn enqueue_tso_packet(queue: &mut DataTxQueue, mut ip_packet: Vec<u8>, gso_size: usize) {
    let Some((ihl, tcp_hdr_len)) = parse_ipv4_tcp_bounds(&ip_packet) else {
        finalize_tcp_checksum_if_needed(&mut ip_packet);
        assert!(
            queue
                .try_push(PacketBuf::from_ip_packet_vec(
                    0,
                    ip_packet,
                    PacketOwner::Ipv4Stack,
                ))
                .is_ok(),
            "smoltcp consumed a TX token without queue capacity"
        );
        return;
    };

    let hdr_len = ihl + tcp_hdr_len;
    let payload_len = ip_packet.len() - hdr_len;
    if gso_size == 0 || payload_len <= gso_size {
        finalize_tcp_checksum_if_needed(&mut ip_packet);
        assert!(
            queue
                .try_push(PacketBuf::from_ip_packet_vec(
                    0,
                    ip_packet,
                    PacketOwner::Ipv4Stack,
                ))
                .is_ok(),
            "smoltcp consumed a TX token without queue capacity"
        );
        return;
    }

    queue.push_tso(TsoSegments {
        ip_packet,
        ihl,
        tcp_hdr_len,
        gso_size,
        offset: 0,
    });
}

/// One admitted large packet, expanded only as the TX consumer makes progress.
pub(crate) struct TsoSegments {
    ip_packet: Vec<u8>,
    ihl: usize,
    tcp_hdr_len: usize,
    gso_size: usize,
    offset: usize,
}

impl TsoSegments {
    pub(crate) fn is_empty(&self) -> bool {
        self.offset == self.ip_packet.len() - self.ihl - self.tcp_hdr_len
    }

    pub(crate) fn pop_front(&mut self) -> PacketBuf {
        let ip_packet = &self.ip_packet;
        let ihl = self.ihl;
        let tcp_hdr_len = self.tcp_hdr_len;
        let hdr_len = ihl + tcp_hdr_len;
        let src = IpAddress::Ipv4(Ipv4Addr::new(
            ip_packet[12],
            ip_packet[13],
            ip_packet[14],
            ip_packet[15],
        ));
        let dst = IpAddress::Ipv4(Ipv4Addr::new(
            ip_packet[16],
            ip_packet[17],
            ip_packet[18],
            ip_packet[19],
        ));
        let base_seq = u32::from_be_bytes([
            ip_packet[ihl + 4],
            ip_packet[ihl + 5],
            ip_packet[ihl + 6],
            ip_packet[ihl + 7],
        ]);

        let headers = &ip_packet[..hdr_len];
        let payload = &ip_packet[hdr_len..];

        let chunk = &payload[self.offset..(self.offset + self.gso_size).min(payload.len())];
        let mut seg = Vec::with_capacity(hdr_len + chunk.len());
        seg.extend_from_slice(headers);
        seg.extend_from_slice(chunk);

        let total_len = (seg.len() as u16).to_be_bytes();
        seg[2..4].copy_from_slice(&total_len);
        seg[10..12].copy_from_slice(&[0, 0]);
        let ip_csum = (!checksum::data(&seg[..ihl])).to_be_bytes();
        seg[10..12].copy_from_slice(&ip_csum);

        let seg_seq = base_seq.wrapping_add(self.offset as u32).to_be_bytes();
        seg[ihl + 4..ihl + 8].copy_from_slice(&seg_seq);
        if self.offset + chunk.len() < payload.len() {
            seg[ihl + 13] &= !(TCP_FLAG_FIN | TCP_FLAG_PSH);
        }
        if self.offset > 0 {
            seg[ihl + 13] &= !TCP_FLAG_CWR;
        }
        seg[ihl + 16..ihl + 18].copy_from_slice(&[0, 0]);
        let tcp_len = (tcp_hdr_len + chunk.len()) as u32;
        let tcp_csum = !checksum::combine(&[
            checksum::pseudo_header(&src, &dst, IpProtocol::Tcp, tcp_len),
            checksum::data(&seg[ihl..]),
        ]);
        seg[ihl + 16..ihl + 18].copy_from_slice(&tcp_csum.to_be_bytes());

        self.offset += chunk.len();
        PacketBuf::from_ip_packet_vec(0, seg, PacketOwner::Ipv4Stack)
    }
}

pub(crate) struct RxToken(PacketBuf);

impl smoltcp::phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self
            .0
            .network_packet()
            .expect("StackDevice RX queue only contains IP packets"))
    }
}

impl smoltcp::phy::Device for StackDevice<'_> {
    type RxToken<'a>
        = RxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.tx.is_full() {
            return None;
        }
        let packet = self.rx.pop_front()?;
        Some((
            RxToken(packet),
            TxToken {
                queue: self.tx,
                gso_size: None,
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.tx.is_full() {
            None
        } else {
            Some(TxToken {
                queue: self.tx,
                gso_size: None,
            })
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps.max_burst_size = Some(SOCKET_BUFFER_SIZE);
        caps.checksum.tcp = Checksum::Rx;
        caps.segmentation.tcpv4 = Some(TSO_MAX_IP_PACKET_LEN);
        caps
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/stack/device/tests.rs");
