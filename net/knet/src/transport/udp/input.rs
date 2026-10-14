// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ::core::net::{SocketAddr, SocketAddrV4};
use etherparse::UdpHeaderSlice;

use super::{
    UDP_HEADER_LEN,
    output::{has_valid_udp_checksum, ipv4_to_core},
    pcb::PreparedUdpPacket,
    registry,
};
use crate::{buf::UdpPacketMetadata, ipv4};

/// Result of UDP input delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputDisposition {
    Accepted,
    NoSocket,
}

/// Delivers a prepared packet through the non-sleeping PCB lookup path.
///
/// `PacketBuf` owns the shared packet allocation before `NetRx` starts, and the
/// PCB receive queue is reserved to its occupancy bound, so this function is
/// allocation-free.
pub(crate) fn deliver_ipv4_packet(
    packet: PreparedUdpPacket,
) -> (InputDisposition, Option<PreparedUdpPacket>) {
    let Some(pcb) = registry::lookup_udp_pcb_on_device(
        packet.local_addr(),
        packet.remote_addr(),
        packet.packet().ifindex(),
    ) else {
        return (InputDisposition::NoSocket, Some(packet));
    };

    pcb.enqueue(packet);
    (InputDisposition::Accepted, None)
}

/// Validates UDP input within an IPv4 view and extracts receive metadata.
///
/// The caller must select an unfragmented IPv4 UDP packet. Attach the returned
/// metadata to the same packet in task context before transferring its ownership
/// to the receive queue.
pub(crate) fn parse_ipv4_datagram(view: ipv4::Ipv4PacketView<'_>) -> Option<UdpPacketMetadata> {
    let header = view.header();
    let udp_packet = view.payload();
    let udp_header = UdpHeaderSlice::from_slice(udp_packet).ok()?;
    let udp_len = udp_header.length() as usize;
    if udp_len < UDP_HEADER_LEN || udp_len > udp_packet.len() {
        return None;
    }

    let udp_packet = &udp_packet[..udp_len];
    if !has_valid_udp_checksum(header.src_addr(), header.dst_addr(), udp_packet) {
        return None;
    }

    let remote = SocketAddr::V4(SocketAddrV4::new(
        ipv4_to_core(header.src_addr()),
        udp_header.source_port(),
    ));
    let local = SocketAddr::V4(SocketAddrV4::new(
        ipv4_to_core(header.dst_addr()),
        udp_header.destination_port(),
    ));

    Some(UdpPacketMetadata::new(
        local,
        remote,
        header.header_len() + UDP_HEADER_LEN,
        header.header_len() + udp_len,
    ))
}
