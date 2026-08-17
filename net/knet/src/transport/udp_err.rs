// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! UDP asynchronous error queue support.

use ::core::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use etherparse::{
    Icmpv4Slice, Icmpv4Type, Icmpv6Slice, Icmpv6Type, UdpHeaderSlice, icmpv6::DestUnreachableCode,
};
use kerrno::LinuxError;

use super::udp::{self, UdpSocketQueuedError};
use crate::{
    SocketErrorInfo, SocketErrorOrigin, icmpv6,
    ipv4::{self, Ipv4Header},
    ipv6::{self, Ipv6Header},
};

const ICMPV4_DST_UNREACHABLE: u8 = 3;
const ICMPV4_TIME_EXCEEDED: u8 = 11;
#[cfg(unittest)]
const ICMPV4_ECHO_REQUEST: u8 = 8;
const ICMPV4_CODE_NET_UNREACHABLE: u8 = 0;
const ICMPV4_CODE_HOST_UNREACHABLE: u8 = 1;
const ICMPV4_CODE_PROTO_UNREACHABLE: u8 = 2;
const ICMPV4_CODE_PORT_UNREACHABLE: u8 = 3;
const ICMPV4_CODE_FRAG_REQUIRED: u8 = 4;
const ICMPV4_CODE_SR_FAILED: u8 = 5;
const ICMPV4_CODE_DST_NET_UNKNOWN: u8 = 6;
const ICMPV4_CODE_DST_HOST_UNKNOWN: u8 = 7;
const ICMPV4_CODE_SRC_HOST_ISOLATED: u8 = 8;
const ICMPV4_CODE_NET_PROHIBITED: u8 = 9;
const ICMPV4_CODE_HOST_PROHIBITED: u8 = 10;
const ICMPV4_CODE_NET_UNREACH_TO_S: u8 = 11;
const ICMPV4_CODE_HOST_UNREACH_TO_S: u8 = 12;
const ICMPV4_CODE_COMM_PROHIBITED: u8 = 13;
const ICMPV4_CODE_HOST_PRECEDENCE: u8 = 14;
const ICMPV4_CODE_PRECEDENCE_CUTOFF: u8 = 15;
const ICMPV4_CODE_TTL_EXPIRED: u8 = 0;
const ICMPV4_CODE_FRAG_EXPIRED: u8 = 1;

pub(crate) type QueuedUdpError = UdpSocketQueuedError;

fn queue_ipv4_error(local: SocketAddrV4, remote: SocketAddrV4, error: QueuedUdpError) {
    if let Some(state) = udp::lookup_udp_error_state(local.into(), remote.into()) {
        if state.peer_endpoint().is_some() {
            state.record_socket_error(error.ancillary.errno);
        }
        state.enqueue_error(error);
    }
}

fn queue_ipv6_error(local: SocketAddrV6, remote: SocketAddrV6, error: QueuedUdpError) {
    if let Some(state) = udp::lookup_udp_error_state(local.into(), remote.into()) {
        if state.peer_endpoint().is_some() {
            state.record_socket_error(error.ancillary.errno);
        }
        state.enqueue_error(error);
    }
}

fn icmpv4_errno(icmp: &Icmpv4Slice<'_>) -> Option<(LinuxError, u32)> {
    let error_type = icmp.type_u8();
    let error_code = icmp.code_u8();
    match error_type {
        ICMPV4_DST_UNREACHABLE => {
            let result = match error_code {
                ICMPV4_CODE_NET_UNREACHABLE => (LinuxError::ENETUNREACH, 0),
                ICMPV4_CODE_HOST_UNREACHABLE => (LinuxError::EHOSTUNREACH, 0),
                ICMPV4_CODE_PROTO_UNREACHABLE => (LinuxError::ENOPROTOOPT, 0),
                ICMPV4_CODE_PORT_UNREACHABLE => (LinuxError::ECONNREFUSED, 0),
                ICMPV4_CODE_FRAG_REQUIRED => {
                    let mtu = match icmp.icmp_type() {
                        Icmpv4Type::DestinationUnreachable(
                            etherparse::icmpv4::DestUnreachableHeader::FragmentationNeeded {
                                next_hop_mtu,
                            },
                        ) => next_hop_mtu as u32,
                        _ => 0,
                    };
                    (LinuxError::EMSGSIZE, mtu)
                }
                ICMPV4_CODE_SR_FAILED => (LinuxError::EOPNOTSUPP, 0),
                ICMPV4_CODE_DST_NET_UNKNOWN => (LinuxError::ENETUNREACH, 0),
                ICMPV4_CODE_DST_HOST_UNKNOWN => (LinuxError::EHOSTDOWN, 0),
                ICMPV4_CODE_SRC_HOST_ISOLATED => (LinuxError::ENONET, 0),
                ICMPV4_CODE_NET_PROHIBITED => (LinuxError::ENETUNREACH, 0),
                ICMPV4_CODE_HOST_PROHIBITED => (LinuxError::EHOSTUNREACH, 0),
                ICMPV4_CODE_NET_UNREACH_TO_S => (LinuxError::ENETUNREACH, 0),
                ICMPV4_CODE_HOST_UNREACH_TO_S => (LinuxError::EHOSTUNREACH, 0),
                ICMPV4_CODE_COMM_PROHIBITED
                | ICMPV4_CODE_HOST_PRECEDENCE
                | ICMPV4_CODE_PRECEDENCE_CUTOFF => (LinuxError::EHOSTUNREACH, 0),
                _ => (LinuxError::EHOSTUNREACH, 0),
            };
            Some(result)
        }
        ICMPV4_TIME_EXCEEDED if error_code == ICMPV4_CODE_TTL_EXPIRED => {
            Some((LinuxError::EHOSTUNREACH, 0))
        }
        ICMPV4_TIME_EXCEEDED if error_code == ICMPV4_CODE_FRAG_EXPIRED => None,
        ICMPV4_TIME_EXCEEDED => Some((LinuxError::EPROTO, 0)),
        _ => None,
    }
}

fn icmpv6_errno(icmp: &Icmpv6Slice<'_>) -> Option<(LinuxError, u32)> {
    match icmp.icmp_type() {
        Icmpv6Type::DestinationUnreachable(code) => {
            let errno = match code {
                DestUnreachableCode::NoRoute => LinuxError::ENETUNREACH,
                DestUnreachableCode::Prohibited => LinuxError::EACCES,
                DestUnreachableCode::BeyondScope => LinuxError::EHOSTUNREACH,
                DestUnreachableCode::Address => LinuxError::EHOSTUNREACH,
                DestUnreachableCode::Port => LinuxError::ECONNREFUSED,
                DestUnreachableCode::SourceAddressFailedPolicy => LinuxError::EACCES,
                DestUnreachableCode::RejectRoute => LinuxError::EACCES,
            };
            Some((errno, 0))
        }
        Icmpv6Type::PacketTooBig { mtu } => Some((LinuxError::EMSGSIZE, mtu)),
        Icmpv6Type::TimeExceeded(_) => Some((LinuxError::EHOSTUNREACH, 0)),
        Icmpv6Type::ParameterProblem(_) => Some((LinuxError::EPROTO, 0)),
        _ => None,
    }
}

/// Inspect an IPv4 ICMP error and queue it for the affected UDP socket.
///
/// ICMP errors quote the packet that triggered the error, so this path parses
/// the outer IPv4 and ICMP headers, then the quoted IPv4 and UDP headers. The
/// quoted UDP 4-tuple determines the receiving socket: a connected socket that
/// matches the peer is preferred, and an unconnected socket bound to the local
/// endpoint is used as a fallback.
///
/// This mirrors Linux's ICMP-to-socket error delivery path in `net/ipv4/icmp.c`
/// and `ip_icmp_error()` in `net/ipv4/ip_sockglue.c`.
pub(crate) fn inspect_icmpv4_error(packet: &[u8]) {
    let ip_packet = match Ipv4Header::parse_input(packet) {
        Ok(header) if header.protocol() == ipv4::PROTOCOL_ICMP => header,
        _ => return,
    };
    let icmp_packet = match ipv4::payload(packet, &ip_packet) {
        Some(payload) => payload,
        _ => return,
    };
    let icmp = match Icmpv4Slice::from_slice(icmp_packet) {
        Ok(icmp) => icmp,
        Err(_) => return,
    };
    if icmp.icmp_type().calc_checksum(icmp.payload()) != icmp.checksum() {
        return;
    }
    let error_type = icmp.type_u8();
    let error_code = icmp.code_u8();
    let Some((errno, info)) = icmpv4_errno(&icmp) else {
        return;
    };

    let original_packet = icmp.payload();
    let original_ip = match Ipv4Header::parse_icmp_quote(original_packet) {
        Ok(header) if header.protocol() == ipv4::PROTOCOL_UDP => header,
        _ => return,
    };
    let original_udp = match original_packet.get(original_ip.header_len()..) {
        Some(payload) => payload,
        _ => return,
    };
    let udp_header = match UdpHeaderSlice::from_slice(original_udp) {
        Ok(header) => header,
        _ => return,
    };

    let local = SocketAddrV4::new(original_ip.src_addr().into(), udp_header.source_port());
    let remote = SocketAddrV4::new(original_ip.dst_addr().into(), udp_header.destination_port());
    // ICMP has no transport-layer port, so Linux reports the offender as the
    // ICMP source address with `sin_port` set to zero.
    let offender = SocketAddr::V4(SocketAddrV4::new(ip_packet.src_addr().into(), 0));
    let error = QueuedUdpError {
        payload: original_udp[udp_header.slice().len()..].to_vec(),
        addr: SocketAddr::V4(remote),
        ancillary: SocketErrorInfo {
            errno,
            origin: SocketErrorOrigin::Icmp,
            error_type,
            error_code,
            info,
            data: 0,
            offender: Some(offender),
        },
    };
    queue_ipv4_error(local, remote, error);
}

/// Inspect an IPv6 ICMPv6 error and queue it for the affected UDP socket.
///
/// Mirrors the IPv4 path: parse outer IPv6 + ICMPv6, validate the checksum,
/// then recover the quoted IPv6 + UDP headers that identify the socket.
///
/// `ifindex` is the interface the error arrived on. Link-local and multicast
/// addresses in the quoted 4-tuple take that index as `sin6_scope_id`, matching
/// Linux `__udp6_lib_err` passing `inet6_iif(skb)` into `__udp6_lib_lookup`.
pub(crate) fn inspect_icmpv6_error(packet: &[u8], ifindex: i32) {
    let ip_packet = match Ipv6Header::parse_input(packet) {
        Ok(header) if header.next_header() == ipv6::PROTOCOL_ICMPV6 => header,
        _ => return,
    };
    let icmp_packet = match ipv6::payload(packet, &ip_packet) {
        Some(payload) => payload,
        _ => return,
    };
    let icmp = match Icmpv6Slice::from_slice(icmp_packet) {
        Ok(icmp) => icmp,
        Err(_) => return,
    };
    if !icmpv6::has_valid_checksum(ip_packet.src_addr(), ip_packet.dst_addr(), icmp_packet) {
        return;
    }
    let error_type = icmp.type_u8();
    let error_code = icmp.code_u8();
    // Only error messages (type < 128) are delivered to sockets.
    if error_type >= 128 {
        return;
    }
    let Some((errno, info)) = icmpv6_errno(&icmp) else {
        return;
    };

    let original_packet = icmp.payload();
    let original_ip = match Ipv6Header::parse_icmp_quote(original_packet) {
        Ok(header) if header.next_header() == ipv6::PROTOCOL_UDP => header,
        _ => return,
    };
    let original_udp = match original_packet.get(original_ip.header_len()..) {
        Some(payload) => payload,
        _ => return,
    };
    let udp_header = match UdpHeaderSlice::from_slice(original_udp) {
        Ok(header) => header,
        _ => return,
    };

    let local = SocketAddrV6::new(
        original_ip.src_addr().into(),
        udp_header.source_port(),
        0,
        original_ip.src_addr().iface_scope_id(ifindex),
    );
    let remote = SocketAddrV6::new(
        original_ip.dst_addr().into(),
        udp_header.destination_port(),
        0,
        original_ip.dst_addr().iface_scope_id(ifindex),
    );
    let offender = SocketAddr::V6(SocketAddrV6::new(
        ip_packet.src_addr().into(),
        0,
        0,
        ip_packet.src_addr().iface_scope_id(ifindex),
    ));
    let payload_start = udp_header.slice().len().min(original_udp.len());
    let error = QueuedUdpError {
        payload: original_udp[payload_start..].to_vec(),
        addr: SocketAddr::V6(remote),
        ancillary: SocketErrorInfo {
            errno,
            origin: SocketErrorOrigin::Icmp,
            error_type,
            error_code,
            info,
            data: 0,
            offender: Some(offender),
        },
    };
    queue_ipv6_error(local, remote, error);
}

#[cfg(unittest)]
mod tests {
    extern crate alloc;

    use alloc::vec;

    use ::core::net::{Ipv4Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
    use unittest::def_test;

    use super::*;
    use crate::{
        SocketErrorOrigin,
        buf::{PacketBuf, PacketOwner, PacketType},
        icmpv6,
        ip::{IpAddress, IpEndpoint, Ipv6Address, Ipv6Cidr},
        ipv4::{Icmpv4Error, Ipv4Header},
        ipv6::{self, Ipv6Header, Ipv6Reassembler, process_ipv6_input},
        udp::{UdpSocketState, register_udp_state_for_test},
    };

    fn endpoint(addr: Ipv4Addr, port: u16) -> IpEndpoint {
        SocketAddrV4::new(addr, port).into()
    }

    fn icmp_packet(ty: u8, code: u8, info: u16) -> [u8; 8] {
        let mut bytes = [0; 8];
        bytes[0] = ty;
        bytes[1] = code;
        bytes[6..8].copy_from_slice(&info.to_be_bytes());
        bytes
    }

    fn assert_icmpv4_errno(ty: u8, code: u8, info: u16, expected: Option<(LinuxError, u32)>) {
        let bytes = icmp_packet(ty, code, info);
        let icmp = Icmpv4Slice::from_slice(&bytes).unwrap();
        assert_eq!(icmpv4_errno(&icmp), expected);
    }

    fn queued_error(errno: LinuxError, payload_byte: u8) -> QueuedUdpError {
        QueuedUdpError {
            payload: vec![payload_byte],
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1234)),
            ancillary: SocketErrorInfo {
                errno,
                origin: SocketErrorOrigin::Icmp,
                error_type: 3,
                error_code: 3,
                info: 0,
                data: 0,
                offender: Some(SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::new(192, 0, 2, 254),
                    0,
                ))),
            },
        }
    }

    fn clear_registry() {
        udp::clear_udp_registry_for_test();
    }

    #[def_test]
    fn test_icmpv4_errno_maps_error_types() {
        assert_icmpv4_errno(
            ICMPV4_DST_UNREACHABLE,
            ICMPV4_CODE_PORT_UNREACHABLE,
            0,
            Some((LinuxError::ECONNREFUSED, 0)),
        );
        assert_icmpv4_errno(
            ICMPV4_DST_UNREACHABLE,
            ICMPV4_CODE_FRAG_REQUIRED,
            1400,
            Some((LinuxError::EMSGSIZE, 1400)),
        );
        assert_icmpv4_errno(
            ICMPV4_DST_UNREACHABLE,
            ICMPV4_CODE_PROTO_UNREACHABLE,
            0,
            Some((LinuxError::ENOPROTOOPT, 0)),
        );
        assert_icmpv4_errno(
            ICMPV4_DST_UNREACHABLE,
            ICMPV4_CODE_SR_FAILED,
            0,
            Some((LinuxError::EOPNOTSUPP, 0)),
        );
        assert_icmpv4_errno(
            ICMPV4_TIME_EXCEEDED,
            ICMPV4_CODE_TTL_EXPIRED,
            0,
            Some((LinuxError::EHOSTUNREACH, 0)),
        );
        assert_icmpv4_errno(ICMPV4_TIME_EXCEEDED, ICMPV4_CODE_FRAG_EXPIRED, 0, None);
        assert_icmpv4_errno(ICMPV4_ECHO_REQUEST, 0, 0, None);
    }

    #[def_test(serial)]
    fn test_connected_udp_error_state_takes_precedence() {
        clear_registry();

        let local = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 8080);
        let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 5353);
        let bound = UdpSocketState::new();
        bound.set_recv_err(true);
        bound.set_local_endpoint(Some(endpoint(*local.ip(), local.port())));
        let connected = UdpSocketState::new();
        connected.set_recv_err(true);
        connected.set_local_endpoint(Some(endpoint(*local.ip(), local.port())));
        connected.set_peer_endpoint(Some((
            endpoint(*remote.ip(), remote.port()),
            IpAddress::Ipv4((*local.ip()).into()),
        )));

        register_udp_state_for_test(bound.clone());
        register_udp_state_for_test(connected.clone());
        queue_ipv4_error(local, remote, queued_error(LinuxError::ECONNREFUSED, 1));

        assert!(!bound.has_pending_error());
        assert!(connected.has_pending_error());

        clear_registry();
    }

    #[def_test(serial)]
    fn test_connected_udp_error_sets_socket_error_without_recverr() {
        clear_registry();

        let local = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 8080);
        let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 5353);
        let connected = UdpSocketState::new();
        connected.set_local_endpoint(Some(endpoint(*local.ip(), local.port())));
        connected.set_peer_endpoint(Some((
            endpoint(*remote.ip(), remote.port()),
            IpAddress::Ipv4((*local.ip()).into()),
        )));

        register_udp_state_for_test(connected.clone());
        queue_ipv4_error(local, remote, queued_error(LinuxError::ECONNREFUSED, 1));

        assert_eq!(
            connected.consume_socket_error(),
            LinuxError::ECONNREFUSED.into_raw()
        );
        assert!(!connected.has_pending_error());

        clear_registry();
    }

    #[def_test(serial)]
    fn test_icmpv4_error_accepts_truncated_original_datagram() {
        clear_registry();

        let local = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 8080);
        let remote = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 5353);
        let connected = UdpSocketState::new();
        connected.set_recv_err(true);
        connected.set_local_endpoint(Some(endpoint(*local.ip(), local.port())));
        connected.set_peer_endpoint(Some((
            endpoint(*remote.ip(), remote.port()),
            IpAddress::Ipv4((*local.ip()).into()),
        )));
        register_udp_state_for_test(connected.clone());

        let mut udp = vec![0u8; 24];
        let udp_len = udp.len() as u16;
        udp[0..2].copy_from_slice(&local.port().to_be_bytes());
        udp[2..4].copy_from_slice(&remote.port().to_be_bytes());
        udp[4..6].copy_from_slice(&udp_len.to_be_bytes());
        let original = ipv4::build_ipv4_packet(
            (*local.ip()).into(),
            (*remote.ip()).into(),
            ipv4::PROTOCOL_UDP,
            64,
            &udp,
        )
        .unwrap();
        let original_header = Ipv4Header::parse_input(&original).unwrap();
        let error = ipv4::build_icmpv4_error_packet(
            Icmpv4Error::PortUnreachable,
            PacketType::Host,
            original_header,
            &original,
        )
        .unwrap();

        inspect_icmpv4_error(&error);

        assert_eq!(
            connected.consume_socket_error(),
            LinuxError::ECONNREFUSED.into_raw()
        );
        let queued = connected.pop_error().unwrap();
        assert!(queued.payload.is_empty());
        assert_eq!(queued.addr, SocketAddr::V4(remote));
        clear_registry();
    }

    #[def_test]
    fn test_udp_error_queue_drops_oldest_entry_when_full() {
        let state = UdpSocketState::new();
        state.set_recv_err(true);

        for byte in 0..33 {
            state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, byte));
        }

        let first = state.pop_error().unwrap();
        assert_eq!(first.payload, vec![1]);
    }

    #[def_test]
    fn test_udp_socket_error_is_read_and_clear() {
        let state = UdpSocketState::new();
        state.set_recv_err(true);
        state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, 1));

        assert_eq!(
            state.consume_socket_error(),
            LinuxError::ECONNREFUSED.into_raw()
        );
        assert_eq!(state.consume_socket_error(), 0);
    }

    fn link_local(last: u8) -> Ipv6Address {
        Ipv6Address::from_octets([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, last])
    }

    fn ipv6_endpoint(addr: Ipv6Address, port: u16, scope_id: u32) -> IpEndpoint {
        SocketAddrV6::new(addr.into(), port, 0, scope_id).into()
    }

    fn icmpv6_port_unreachable(
        local: Ipv6Address,
        remote: Ipv6Address,
        local_port: u16,
        remote_port: u16,
    ) -> alloc::vec::Vec<u8> {
        let mut udp = [0u8; 8];
        udp[0..2].copy_from_slice(&local_port.to_be_bytes());
        udp[2..4].copy_from_slice(&remote_port.to_be_bytes());
        udp[4..6].copy_from_slice(&8u16.to_be_bytes());
        let original =
            ipv6::build_ipv6_packet(local, remote, ipv6::PROTOCOL_UDP, 64, &udp).unwrap();
        let original_header = Ipv6Header::parse_input(&original).unwrap();
        icmpv6::build_icmpv6_error_packet(
            icmpv6::Icmpv6Error::PortUnreachable,
            PacketType::Host,
            original_header,
            &original,
        )
        .unwrap()
    }

    #[def_test(serial)]
    fn test_icmpv6_error_input_delivers_to_scoped_link_local_socket() {
        clear_registry();

        let local_addr = link_local(1);
        let remote_addr = link_local(2);
        let ifindex = 2;
        let connected = UdpSocketState::new();
        connected.set_local_endpoint(Some(ipv6_endpoint(local_addr, 8080, ifindex as u32)));
        connected.set_peer_endpoint(Some((
            ipv6_endpoint(remote_addr, 5353, ifindex as u32),
            IpAddress::Ipv6(local_addr),
        )));
        register_udp_state_for_test(connected.clone());

        let error = icmpv6_port_unreachable(local_addr, remote_addr, 8080, 5353);
        let packet = PacketBuf::from_ip_packet_vec(ifindex, error, PacketOwner::DeviceRx);
        let local_addrs = [Ipv6Cidr::new(local_addr, 128)];
        let mut control_packets = alloc::vec::Vec::new();
        let mut actions = alloc::vec::Vec::new();
        let mut reassembler = Ipv6Reassembler::new();
        let now = ktime_types::MonotonicInstant::from_span_since_origin(
            ktime_types::TimeSpan::from_secs(1),
        );

        assert!(
            process_ipv6_input(
                now,
                packet,
                &local_addrs,
                &mut reassembler,
                &mut control_packets,
                &mut actions,
            )
            .is_none()
        );
        assert_eq!(
            connected.consume_socket_error(),
            LinuxError::ECONNREFUSED.into_raw()
        );

        clear_registry();
    }

    #[def_test(serial)]
    fn test_icmpv6_error_lookup_requires_arrival_scope() {
        clear_registry();

        let local_addr = link_local(1);
        let remote_addr = link_local(2);
        let bound_scope = 2;
        let connected = UdpSocketState::new();
        connected.set_local_endpoint(Some(ipv6_endpoint(local_addr, 8080, bound_scope)));
        connected.set_peer_endpoint(Some((
            ipv6_endpoint(remote_addr, 5353, bound_scope),
            IpAddress::Ipv6(local_addr),
        )));
        register_udp_state_for_test(connected.clone());

        let error = icmpv6_port_unreachable(local_addr, remote_addr, 8080, 5353);
        inspect_icmpv6_error(&error, 3);
        assert_eq!(connected.consume_socket_error(), 0);

        inspect_icmpv6_error(&error, bound_scope as i32);
        assert_eq!(
            connected.consume_socket_error(),
            LinuxError::ECONNREFUSED.into_raw()
        );

        clear_registry();
    }
}
