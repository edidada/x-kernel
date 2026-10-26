// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ::core::net::{SocketAddr, SocketAddrV4};
use etherparse::{Ipv4HeaderSlice, UdpHeaderSlice};

use super::{UDP_HEADER_LEN, output::has_valid_udp_checksum, pcb::PreparedUdpPacket, registry};
use crate::{
    buf::{PacketBuf, UdpPacketMetadata},
    ipv4,
};

/// Result of UDP input delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputDisposition {
    Accepted,
    NoSocket,
}

/// Stamps metadata produced by `parse_ipv4_packet` before transferring ownership.
/// This runs in task context before loopback disables BH.
pub(crate) fn prepare_ipv4_packet(
    metadata: UdpPacketMetadata,
    packet: PacketBuf,
) -> Result<PreparedUdpPacket, PacketBuf> {
    debug_assert!(
        !kirq::context::is_in_interrupt_context(),
        "prepare_ipv4_packet requires task context"
    );
    let range = metadata.payload_range();
    PreparedUdpPacket::new(
        packet,
        range.start,
        range.len(),
        metadata.local_addr(),
        metadata.remote_addr(),
    )
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

/// Validates UDP using the caller's already decoded IPv4 header.
pub(crate) fn parse_ipv4_packet(
    header: &Ipv4HeaderSlice<'_>,
    ip_packet: &[u8],
) -> Option<UdpPacketMetadata> {
    if header.protocol().0 != ipv4::PROTOCOL_UDP || header.is_fragmenting_payload() {
        return None;
    }
    let udp_packet = ipv4::payload(ip_packet, header)?;
    let udp_header = UdpHeaderSlice::from_slice(udp_packet).ok()?;
    let udp_len = udp_header.length() as usize;
    if udp_len < UDP_HEADER_LEN || udp_len > udp_packet.len() {
        return None;
    }

    let udp_packet = &udp_packet[..udp_len];
    if !has_valid_udp_checksum(
        header.source_addr().into(),
        header.destination_addr().into(),
        udp_packet,
    ) {
        return None;
    }

    let remote = SocketAddr::V4(SocketAddrV4::new(
        header.source_addr(),
        udp_header.source_port(),
    ));
    let local = SocketAddr::V4(SocketAddrV4::new(
        header.destination_addr(),
        udp_header.destination_port(),
    ));

    let payload_start = header.slice().len() + UDP_HEADER_LEN;
    Some(UdpPacketMetadata::new(
        local,
        remote,
        payload_start,
        payload_start + udp_len - UDP_HEADER_LEN,
    ))
}
