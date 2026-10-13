// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TCP listener preparation before accepted packets enter smoltcp.

use smoltcp::{
    iface::SocketSet,
    wire::{IpAddress as SmoltcpIpAddress, IpProtocol, Ipv6Packet, TcpPacket},
};

use crate::{
    LISTEN_TABLE,
    buf::PacketBuf,
    network::ipv4::{self, Ipv4Header},
};

/// Updates TCP listener state before accepted packets enter smoltcp.
///
/// UDP delivery has already completed before this function takes the shared
/// smoltcp socket-set lock. This keeps custom UDP ingress independent from
/// unrelated TCP socket operations while preserving listener setup before a
/// TCP SYN reaches smoltcp.
pub(crate) fn prepare_smoltcp_ingress(packets: &[PacketBuf], sockets: &mut SocketSet<'_>) {
    for packet in packets {
        let Some(ip_packet) = packet.network_packet() else {
            continue;
        };
        match ipv4::ip_version(ip_packet) {
            Some(4) => {
                let Ok(header) = Ipv4Header::parse_input(ip_packet) else {
                    continue;
                };
                let Some(payload) = ipv4::payload(ip_packet, &header) else {
                    continue;
                };
                snoop_tcp_segment(
                    header.protocol(),
                    SmoltcpIpAddress::Ipv4(header.src_addr().into()),
                    SmoltcpIpAddress::Ipv4(header.dst_addr().into()),
                    payload,
                    sockets,
                );
            }
            Some(6) => snoop_ipv6_tcp_packet(ip_packet, sockets),
            _ => {}
        }
    }
}

fn snoop_ipv6_tcp_packet(buf: &[u8], sockets: &mut SocketSet<'_>) {
    let Ok(ip_packet) = Ipv6Packet::new_checked(buf) else {
        return;
    };
    // TODO: Traverse IPv6 extension headers before TCP snooping.
    // `next_header` and `payload` only refer to the base IPv6 header.
    let protocol = match ip_packet.next_header() {
        IpProtocol::Tcp => ipv4::PROTOCOL_TCP,
        _ => return,
    };
    snoop_tcp_segment(
        protocol,
        SmoltcpIpAddress::Ipv6(ip_packet.src_addr()),
        SmoltcpIpAddress::Ipv6(ip_packet.dst_addr()),
        ip_packet.payload(),
        sockets,
    );
}

fn snoop_tcp_segment(
    protocol: u8,
    src_addr: SmoltcpIpAddress,
    dst_addr: SmoltcpIpAddress,
    payload: &[u8],
    sockets: &mut SocketSet<'_>,
) {
    if protocol != ipv4::PROTOCOL_TCP {
        return;
    }

    let Ok(tcp_packet) = TcpPacket::new_checked(payload) else {
        return;
    };
    let src_addr = (src_addr, tcp_packet.src_port()).into();
    let dst_addr = (dst_addr, tcp_packet.dst_port()).into();
    LISTEN_TABLE.note_tcp_packet(dst_addr);
    if tcp_packet.syn() && !tcp_packet.ack() {
        LISTEN_TABLE.incoming_tcp_packet(src_addr, dst_addr, sockets);
    }
}
