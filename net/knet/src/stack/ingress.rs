// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network-layer ingress processing outside the router lock.

use alloc::{string::String, vec::Vec};

use ktime_types::MonotonicInstant;
use smoltcp::{
    iface::SocketSet,
    wire::{IpAddress as SmoltcpIpAddress, IpProtocol, Ipv6Packet, TcpPacket},
};

use crate::{
    LISTEN_TABLE,
    buf::{PacketBuf, PacketType},
    ip::{Ipv4Address, Ipv4Cidr},
    netfilter::{FilterHook, Verdict, evaluate_ipv4},
    network::{
        fragment::{Ipv4Reassembler, Ipv4ReassemblyResult},
        ipv4::{self, Icmpv4Error, Ipv4Error, Ipv4Header},
    },
    udp, udp_err,
};

pub(crate) struct IngressProcessor {
    local_ipv4_addrs: Vec<Ipv4Cidr>,
    device_names: Vec<String>,
    ipv4_reassembler: Ipv4Reassembler,
}

impl IngressProcessor {
    pub fn new() -> Self {
        Self {
            local_ipv4_addrs: Vec::new(),
            device_names: Vec::new(),
            ipv4_reassembler: Ipv4Reassembler::new(),
        }
    }

    pub fn update_local_ipv4_addrs(&mut self, addrs: &[Ipv4Cidr]) {
        self.local_ipv4_addrs.clear();
        self.local_ipv4_addrs.extend_from_slice(addrs);
    }

    pub(crate) fn update_device_names(&mut self, names: Vec<String>) {
        self.device_names = names;
    }

    pub(crate) fn ipv4_reassembly_deadline(&self) -> Option<MonotonicInstant> {
        self.ipv4_reassembler.expiration_deadline()
    }

    pub(crate) fn expire_ipv4_fragments(
        &mut self,
        timestamp: MonotonicInstant,
        timer_budget: usize,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> usize {
        let expired = self
            .ipv4_reassembler
            .remove_expired(timestamp, timer_budget);
        for fragment in expired.fragments {
            queue_icmpv4_error(
                control_packets,
                Icmpv4Error::FragmentReassemblyTimeout,
                fragment.packet_type,
                fragment.header,
                &fragment.packet,
            );
        }
        expired.timer_events
    }

    /// Consumes `packets` and appends work for the shared stack and control TX.
    ///
    /// The output batches may contain work retained from an earlier poll round
    /// whose shared destination lock was contended.
    pub fn handle_rx_packets(
        &mut self,
        timestamp: MonotonicInstant,
        packets: &mut Vec<PacketBuf>,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
        forwarded_packets: &mut Vec<PacketBuf>,
    ) {
        for packet in packets.drain(..) {
            self.handle_rx_packet(
                timestamp,
                packet,
                accepted_packets,
                control_packets,
                forwarded_packets,
            );
        }
    }

    fn handle_rx_packet(
        &mut self,
        timestamp: MonotonicInstant,
        packet: PacketBuf,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
        forwarded_packets: &mut Vec<PacketBuf>,
    ) {
        let Some(ip_packet) = packet.network_packet() else {
            return;
        };
        match ipv4::ip_version(ip_packet) {
            Some(4) => self.handle_ipv4_input(
                timestamp,
                packet,
                accepted_packets,
                control_packets,
                forwarded_packets,
            ),
            Some(6) => {
                accepted_packets.push(packet);
            }
            _ => debug!("Dropping packet with invalid IP version"),
        }
    }

    fn handle_ipv4_input(
        &mut self,
        timestamp: MonotonicInstant,
        mut packet: PacketBuf,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
        forwarded_packets: &mut Vec<PacketBuf>,
    ) {
        let header = match Ipv4Header::validate_input_packet(&mut packet) {
            Ok(header) => header,
            Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return,
        };

        if !self.is_local_ipv4_destination(header.dst_addr()) {
            if packet.packet_type() == PacketType::Host
                && !header.dst_addr().is_broadcast()
                && !header.dst_addr().is_multicast()
            {
                forwarded_packets.push(packet);
            }
            return;
        }

        if header.is_fragmented() {
            match self.ipv4_reassembler.reassemble(packet, header, timestamp) {
                Ipv4ReassemblyResult::Complete(packet) => self.handle_ipv4_input(
                    timestamp,
                    packet,
                    accepted_packets,
                    control_packets,
                    forwarded_packets,
                ),
                Ipv4ReassemblyResult::Pending | Ipv4ReassemblyResult::Dropped => {}
            }
            return;
        }

        let indev = packet
            .ifindex()
            .checked_sub(1)
            .and_then(|index| self.device_names.get(index as usize))
            .map_or("", String::as_str);
        if !packet.input_filtered()
            && evaluate_ipv4(
                FilterHook::Input,
                packet.network_packet().unwrap(),
                indev,
                "",
            ) != Verdict::Accept
        {
            return;
        }

        if header.protocol() == ipv4::PROTOCOL_UDP {
            let packet_type = packet.packet_type();
            let packet = match udp::prepare_ipv4_packet(header, packet) {
                Ok(packet) => packet,
                Err(_) => return,
            };
            let (disposition, returned_packet) = udp::deliver_ipv4_packet(packet);
            match disposition {
                udp::InputDisposition::Accepted => return,
                udp::InputDisposition::NoSocket if !header.is_broadcast_or_multicast() => {
                    let Some(packet) = returned_packet else {
                        return;
                    };
                    let Some(ip_packet) = packet.packet().network_packet() else {
                        return;
                    };
                    queue_icmpv4_error(
                        control_packets,
                        Icmpv4Error::PortUnreachable,
                        packet_type,
                        header,
                        ip_packet,
                    );
                    return;
                }
                udp::InputDisposition::NoSocket => return,
            }
        }

        if should_emit_protocol_unreachable(header) {
            if let Some(ip_packet) = packet.network_packet() {
                queue_icmpv4_error(
                    control_packets,
                    Icmpv4Error::ProtocolUnreachable,
                    packet.packet_type(),
                    header,
                    ip_packet,
                );
            }
            return;
        }

        if let Some(ip_packet) = packet.network_packet() {
            udp_err::inspect_icmpv4_error(ip_packet, packet.ifindex());
        }
        accepted_packets.push(packet);
    }

    fn is_local_ipv4_destination(&self, dst_addr: Ipv4Address) -> bool {
        dst_addr.is_broadcast()
            || dst_addr.is_multicast()
            || self
                .local_ipv4_addrs
                .iter()
                .any(|addr| addr.address() == dst_addr || addr.broadcast() == Some(dst_addr))
    }
}

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

fn queue_icmpv4_error(
    control_packets: &mut Vec<Vec<u8>>,
    error: Icmpv4Error,
    packet_type: crate::buf::PacketType,
    offending_header: Ipv4Header,
    offending_packet: &[u8],
) {
    let Some(packet) =
        ipv4::build_icmpv4_error_packet(error, packet_type, offending_header, offending_packet)
    else {
        return;
    };
    control_packets.push(packet);
}

fn should_emit_protocol_unreachable(header: Ipv4Header) -> bool {
    !header.is_fragmented()
        && !header.is_broadcast_or_multicast()
        && !matches!(
            header.protocol(),
            ipv4::PROTOCOL_TCP | ipv4::PROTOCOL_UDP | ipv4::PROTOCOL_ICMP
        )
}

#[cfg(unittest)]
mod forwarding_tests {
    use alloc::{vec, vec::Vec};

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::buf::{PacketOwner, PacketType};

    #[def_test]
    fn nonlocal_ipv4_packet_enters_forwarding_batch() {
        let mut ingress = IngressProcessor::new();
        ingress
            .update_local_ipv4_addrs(&[Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8).unwrap()]);
        let bytes = ipv4::build_ipv4_packet(
            Ipv4Address::new(203, 0, 113, 9),
            Ipv4Address::new(198, 51, 100, 2),
            ipv4::PROTOCOL_ICMP,
            64,
            &[8, 0, 0, 0, 0, 0, 0, 0],
        )
        .unwrap();
        let mut input = vec![PacketBuf::from_ip_packet_vec(
            1,
            bytes,
            PacketOwner::DeviceRx,
        )];
        let mut accepted = Vec::new();
        let mut control = Vec::new();
        let mut forwarded = Vec::new();

        ingress.handle_rx_packets(
            MonotonicInstant::ORIGIN,
            &mut input,
            &mut accepted,
            &mut control,
            &mut forwarded,
        );

        assert_eq!(input.len(), 0);
        assert_eq!(accepted.len(), 0);
        assert_eq!(control.len(), 0);
        assert_eq!(forwarded.len(), 1);
    }

    #[def_test]
    fn link_broadcast_with_unicast_ipv4_destination_is_not_forwarded() {
        let mut ingress = IngressProcessor::new();
        ingress
            .update_local_ipv4_addrs(&[Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8).unwrap()]);
        let bytes = ipv4::build_ipv4_packet(
            Ipv4Address::new(203, 0, 113, 9),
            Ipv4Address::new(198, 51, 100, 2),
            ipv4::PROTOCOL_ICMP,
            64,
            &[8, 0, 0, 0, 0, 0, 0, 0],
        )
        .unwrap();
        let mut input = vec![PacketBuf::from_ip_packet_vec_with_type(
            1,
            bytes,
            PacketOwner::DeviceRx,
            PacketType::Broadcast,
        )];
        let mut accepted = Vec::new();
        let mut control = Vec::new();
        let mut forwarded = Vec::new();

        ingress.handle_rx_packets(
            MonotonicInstant::ORIGIN,
            &mut input,
            &mut accepted,
            &mut control,
            &mut forwarded,
        );

        assert_eq!(accepted.len(), 0);
        assert_eq!(control.len(), 0);
        assert_eq!(forwarded.len(), 0);
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
