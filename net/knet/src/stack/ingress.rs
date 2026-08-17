// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network-layer ingress processing outside the router lock.

use alloc::vec::Vec;

use ktime_types::MonotonicInstant;
use smoltcp::{
    iface::SocketSet,
    wire::{IpAddress as SmoltcpIpAddress, TcpPacket},
};

use super::{
    ipv4::{self, Icmpv4Error, Ipv4Header, fragment::Ipv4Reassembler},
    ipv6::{self, Ipv6Action, Ipv6Header, Ipv6Reassembler, icmpv6::Icmpv6Error},
};
use crate::{
    LISTEN_TABLE,
    buf::PacketBuf,
    ip::{Ipv4Cidr, Ipv6Cidr},
    udp_err,
};

pub(crate) struct IngressProcessor {
    local_ipv4_addrs: Vec<Ipv4Cidr>,
    local_ipv6_addrs: Vec<Ipv6Cidr>,
    ipv4_reassembler: Ipv4Reassembler,
    ipv6_reassembler: Ipv6Reassembler,
}

impl IngressProcessor {
    pub fn new() -> Self {
        Self {
            local_ipv4_addrs: Vec::new(),
            local_ipv6_addrs: Vec::new(),
            ipv4_reassembler: Ipv4Reassembler::new(),
            ipv6_reassembler: Ipv6Reassembler::new(),
        }
    }

    pub fn update_local_ipv4_addrs(&mut self, addrs: &[Ipv4Cidr]) {
        self.local_ipv4_addrs.clear();
        self.local_ipv4_addrs.extend_from_slice(addrs);
    }

    pub fn update_local_ipv6_addrs(&mut self, addrs: &[Ipv6Cidr]) {
        self.local_ipv6_addrs.clear();
        self.local_ipv6_addrs.extend_from_slice(addrs);
    }

    pub fn handle_rx_packets(
        &mut self,
        timestamp: MonotonicInstant,
        packets: &mut Vec<PacketBuf>,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
        ipv6_actions: &mut Vec<Ipv6Action>,
        sockets: &mut SocketSet<'_>,
    ) {
        accepted_packets.clear();
        for expired in self.ipv4_reassembler.remove_expired(timestamp) {
            ipv4::queue_icmpv4_error(
                control_packets,
                Icmpv4Error::FragmentReassemblyTimeout,
                expired.packet_type,
                expired.header,
                &expired.packet,
            );
        }
        for expired in self.ipv6_reassembler.remove_expired(timestamp) {
            ipv6::queue_icmpv6_error(
                control_packets,
                Icmpv6Error::FragmentReassemblyTimeout,
                expired.packet_type,
                expired.header,
                &expired.packet,
            );
        }
        for packet in packets.drain(..) {
            self.handle_rx_packet(
                timestamp,
                packet,
                accepted_packets,
                control_packets,
                ipv6_actions,
                sockets,
            );
        }
    }

    fn handle_rx_packet(
        &mut self,
        timestamp: MonotonicInstant,
        packet: PacketBuf,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
        ipv6_actions: &mut Vec<Ipv6Action>,
        sockets: &mut SocketSet<'_>,
    ) {
        let Some(ip_packet) = packet.network_packet() else {
            return;
        };
        match ipv4::ip_version(ip_packet) {
            Some(4) => {
                let Some(packet) = ipv4::process_ipv4_input(
                    timestamp,
                    packet,
                    &self.local_ipv4_addrs,
                    &mut self.ipv4_reassembler,
                    control_packets,
                ) else {
                    return;
                };
                if let Some(ip_packet) = packet.network_packet() {
                    udp_err::inspect_icmpv4_error(ip_packet);
                    snoop_tcp_packet(ip_packet, sockets);
                }
                accepted_packets.push(packet);
            }
            Some(6) => {
                let Some(packet) = ipv6::process_ipv6_input(
                    timestamp,
                    packet,
                    &self.local_ipv6_addrs,
                    &mut self.ipv6_reassembler,
                    control_packets,
                    ipv6_actions,
                ) else {
                    return;
                };
                if let Some(ip_packet) = packet.network_packet() {
                    snoop_tcp_packet(ip_packet, sockets);
                }
                accepted_packets.push(packet);
            }
            _ => debug!("Dropping packet with invalid IP version"),
        }
    }
}

fn parse_ip_packet(buf: &[u8]) -> Option<(u8, SmoltcpIpAddress, SmoltcpIpAddress, &[u8])> {
    match ipv4::ip_version(buf)? {
        4 => {
            let header = Ipv4Header::parse_input(buf).ok()?;
            let payload = ipv4::payload(buf, &header)?;
            Some((
                header.protocol(),
                SmoltcpIpAddress::Ipv4(header.src_addr().into()),
                SmoltcpIpAddress::Ipv4(header.dst_addr().into()),
                payload,
            ))
        }
        6 => {
            let header = Ipv6Header::parse_input(buf).ok()?;
            let payload = ipv6::payload(buf, &header)?;
            Some((
                header.next_header(),
                SmoltcpIpAddress::Ipv6(header.src_addr().into()),
                SmoltcpIpAddress::Ipv6(header.dst_addr().into()),
                payload,
            ))
        }
        _ => None,
    }
}

fn snoop_tcp_packet(buf: &[u8], sockets: &mut SocketSet<'_>) {
    let Some((protocol, src_addr, dst_addr, payload)) = parse_ip_packet(buf) else {
        return;
    };
    if protocol != ipv4::PROTOCOL_TCP && protocol != ipv6::PROTOCOL_TCP {
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
