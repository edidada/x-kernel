// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::{boxed::Box, format, string::String, vec, vec::Vec};

use ::core::{sync::atomic::Ordering, task::Waker};
use kerrno::LinuxError;
use khal::time::TimeValue;
use kio::Cursor;
use unittest::def_test;

use super::{
    route::*,
    socket::publish_kobject_uevent,
    wire::{
        NlMsgHeader, RtMsg, addr as wire_addr, build_nlmsg, link as wire_link, neigh as wire_neigh,
        push_attr, push_i32_ne, push_u8, push_u16_ne, push_u32_ne, read_i32_ne, read_u16_ne,
        route as wire_route,
    },
    *,
};
use crate::{
    RecvOptions, SERVICE, SocketAddrEx,
    buf::PacketBuf,
    device::{
        LINK_FLAG_BROADCAST, LINK_FLAG_LOOPBACK, LINK_FLAG_LOWER_UP, LINK_FLAG_MULTICAST,
        LINK_FLAG_RUNNING, LINK_FLAG_UP, LinkKind, LinkSnapshot, NeighborUpdate, NetDevice,
    },
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address},
    router::{
        Ipv4AddrEntry, ROUTE_PROTOCOL_BOOT, ROUTE_SCOPE_HOST, ROUTE_SCOPE_UNIVERSE,
        ROUTE_TABLE_MAIN, ROUTE_TYPE_UNICAST, RouteAttrs, Router, Rule,
    },
    service::Service,
    socket::SocketOps,
};

struct TestDevice {
    link: LinkSnapshot,
    neighbors: Vec<NeighborUpdate>,
}

impl TestDevice {
    fn new(link: LinkSnapshot) -> Self {
        Self {
            link,
            neighbors: Vec::new(),
        }
    }
}

impl NetDevice for TestDevice {
    fn name(&self) -> &str {
        &self.link.name
    }

    fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot {
        LinkSnapshot {
            ifindex,
            ..self.link.clone()
        }
    }

    fn poll_rx(&mut self, _ifindex: i32, _timestamp: TimeValue) -> Option<PacketBuf> {
        None
    }

    fn send_ip_packet(
        &mut self,
        _ifindex: i32,
        _next_hop: IpAddress,
        _packet: PacketBuf,
        _timestamp: TimeValue,
    ) -> bool {
        false
    }

    fn register_rx_waker(&self, _waker: &Waker) {}

    fn set_name(&mut self, name: String) {
        self.link.name = name;
    }

    fn set_mtu(&mut self, mtu: usize) {
        self.link.mtu = mtu;
    }

    fn set_link_up(&mut self, is_up: bool) {
        if is_up {
            self.link.flags |= LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP;
            self.link.operstate = 6;
        } else {
            self.link.flags &= !(LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP);
            self.link.operstate = 2;
        }
    }

    fn apply_neighbor_update(&mut self, update: NeighborUpdate) {
        if let Some(existing) = self
            .neighbors
            .iter_mut()
            .find(|neighbor| neighbor.dst == update.dst)
        {
            *existing = update;
        } else {
            self.neighbors.push(update);
        }
    }

    fn has_neighbor(&self, dst: IpAddress) -> bool {
        self.neighbors.iter().any(|neighbor| neighbor.dst == dst)
    }

    fn neighbor_snapshots(&self, _dev: usize) -> Vec<NeighborUpdate> {
        self.neighbors.clone()
    }
}

fn init_test_state() {
    let mut router = Router::new();
    let lo_ip = Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8);
    let lo_dev = router.add_device(Box::new(TestDevice::new(LinkSnapshot {
        ifindex: 1,
        name: String::from("lo"),
        flags: LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOOPBACK | LINK_FLAG_LOWER_UP,
        mtu: 65_536,
        operstate: 0,
        kind: LinkKind::Loopback,
        hardware_addr: [0; 6],
        broadcast_addr: [0; 6],
    })));
    router.add_ipv4_addr(Ipv4AddrEntry {
        dev: lo_dev,
        addr: lo_ip,
        scope: ROUTE_SCOPE_HOST,
    });
    router.add_route_rule(Rule::with_route_attrs(
        lo_ip.into(),
        None,
        lo_dev,
        lo_ip.address().into(),
        65_536,
        route_attrs(ROUTE_SCOPE_HOST, Some(lo_ip.address().into())),
    ));

    let eth0_ip = Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 2), 24);
    let eth0_dev = router.add_device(Box::new(TestDevice::new(LinkSnapshot {
        ifindex: 2,
        name: String::from("eth0"),
        flags: LINK_FLAG_UP
            | LINK_FLAG_RUNNING
            | LINK_FLAG_BROADCAST
            | LINK_FLAG_MULTICAST
            | LINK_FLAG_LOWER_UP,
        mtu: 1500,
        operstate: 6,
        kind: LinkKind::Ethernet,
        hardware_addr: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
        broadcast_addr: [0xff; 6],
    })));
    router.add_ipv4_addr(Ipv4AddrEntry {
        dev: eth0_dev,
        addr: eth0_ip,
        scope: ROUTE_SCOPE_UNIVERSE,
    });
    router.add_route_rule(Rule::with_route_attrs(
        Ipv4Cidr::new(eth0_ip.address(), 32).into(),
        None,
        lo_dev,
        eth0_ip.address().into(),
        65_536,
        route_attrs(ROUTE_SCOPE_HOST, Some(eth0_ip.address().into())),
    ));
    router.add_route_rule(Rule::with_route_attrs(
        Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
        Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1))),
        eth0_dev,
        eth0_ip.address().into(),
        1500,
        route_attrs(ROUTE_SCOPE_UNIVERSE, Some(eth0_ip.address().into())),
    ));

    if SERVICE.is_inited() {
        SERVICE.lock().replace_router_for_tests(router);
    } else {
        SERVICE.init_once(ksync::Mutex::new(Service::new(router)));
    }
}

fn route_attrs(scope: u8, prefsrc: Option<IpAddress>) -> RouteAttrs {
    RouteAttrs {
        table: ROUTE_TABLE_MAIN,
        protocol: ROUTE_PROTOCOL_BOOT,
        scope,
        route_type: ROUTE_TYPE_UNICAST,
        prefsrc,
    }
}

fn nl_header(msg_type: u16, flags: u16, seq: u32) -> NlMsgHeader {
    NlMsgHeader {
        len: NLMSG_HDR_LEN as u32,
        msg_type,
        flags,
        seq,
        pid: 0,
    }
}

fn attr(kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    push_attr(&mut out, kind, payload);
    out
}

#[def_test(serial)]
fn test_owner_snapshot_contains_links_and_routes() {
    init_test_state();
    let state = SERVICE.lock().network_snapshot();

    assert_eq!(state.links.len(), 2);
    assert_eq!(state.addrs.len(), 2);
    assert_eq!(state.links[0].name, "lo");
    assert_eq!(state.links[1].name, "eth0");
    assert!(state.routes.iter().any(|route| {
        route.filter == IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8))
            && route.scope == wire_route::SCOPE_HOST
            && route.via.is_none()
    }));
    assert!(state.routes.iter().any(|route| {
        route.filter == IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 2), 32))
            && route.scope == wire_route::SCOPE_HOST
            && route.via.is_none()
    }));
    assert!(state.routes.iter().any(|route| {
        route.filter.prefix_len() == 0
            && route.scope == wire_route::SCOPE_UNIVERSE
            && route.via == Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1)))
    }));
}

#[def_test(serial)]
fn test_parse_link_update_name_and_mtu() {
    let mut payload = Vec::new();
    push_u8(&mut payload, 17);
    push_u8(&mut payload, 0);
    push_u16_ne(&mut payload, ARPHRD_ETHER);
    push_i32_ne(&mut payload, 2);
    push_u32_ne(&mut payload, LINK_FLAG_UP);
    push_u32_ne(&mut payload, LINK_FLAG_UP);
    payload.extend(attr(wire_link::attr::IFNAME, b"eth1\0"));
    payload.extend(attr(wire_link::attr::MTU, &1400u32.to_ne_bytes()));

    let request = parse_link_update(&payload).unwrap();
    assert_eq!(request.index, 2);
    assert_eq!(request.name, Some(String::from("eth1")));
    assert_eq!(request.mtu, Some(1400));
}

#[def_test(serial)]
fn test_update_link_state_rename_and_down() {
    init_test_state();
    let request = LinkUpdateRequest {
        index: 2,
        flags: 0,
        change: LINK_FLAG_UP,
        name: Some(String::from("ens3")),
        mtu: Some(1400),
    };
    update_link_state(request).unwrap();

    let state = SERVICE.lock().network_snapshot();
    let link = state.links.iter().find(|link| link.ifindex == 2).unwrap();
    assert_eq!(link.name, "ens3");
    assert_eq!(link.mtu, 1400);
    assert_eq!(link.flags & LINK_FLAG_UP, 0);
}

#[def_test(serial)]
fn test_add_replace_and_delete_address() {
    init_test_state();
    let req = AddrRequest {
        index: 2,
        family: wire_route::FAMILY_IPV4,
        prefix_len: 24,
        scope: wire_route::SCOPE_UNIVERSE,
        address: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 99)),
    };

    add_addr_state(req, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert!(
        SERVICE
            .lock()
            .network_snapshot()
            .addrs
            .iter()
            .any(|addr| IpAddress::Ipv4(addr.addr.address()) == req.address && addr.dev == 1)
    );

    let replaced = AddrRequest {
        scope: wire_route::SCOPE_HOST,
        ..req
    };
    add_addr_state(replaced, NLM_F_REPLACE).unwrap();
    assert!(
        SERVICE
            .lock()
            .network_snapshot()
            .addrs
            .iter()
            .any(
                |addr| IpAddress::Ipv4(addr.addr.address()) == replaced.address
                    && addr.scope == wire_route::SCOPE_HOST
            )
    );

    del_addr_state(replaced).unwrap();
    assert!(
        !SERVICE
            .lock()
            .network_snapshot()
            .addrs
            .iter()
            .any(|addr| IpAddress::Ipv4(addr.addr.address()) == replaced.address && addr.dev == 1)
    );
}

#[def_test(serial)]
fn test_add_and_delete_route() {
    init_test_state();
    let req = RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        table: u32::from(ROUTE_TABLE_MAIN),
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: ROUTE_TYPE_UNICAST,
        oif: 2,
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(172, 16, 0, 0))),
        gateway: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1))),
        prefsrc: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2))),
    };

    add_route_state(req, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert!(
        SERVICE
            .lock()
            .network_snapshot()
            .routes
            .iter()
            .any(|route| route.filter.address() == req.dst.unwrap() && route.via == req.gateway)
    );

    del_route_state(req).unwrap();
    assert!(
        !SERVICE
            .lock()
            .network_snapshot()
            .routes
            .iter()
            .any(|route| route.filter.address() == req.dst.unwrap() && route.via == req.gateway)
    );
}

#[def_test(serial)]
fn test_add_and_replace_neighbour() {
    init_test_state();
    let req = NeighRequest {
        family: wire_route::FAMILY_IPV4,
        ifindex: 2,
        state: wire_neigh::STATE_PERMANENT,
        flags: 0,
        dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 10)),
        lladdr: Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
    };

    add_neigh_state(req, NLM_F_CREATE | NLM_F_REPLACE).unwrap();
    assert!(
        SERVICE
            .lock()
            .network_snapshot()
            .neighbors
            .iter()
            .any(|neigh| neigh.dst == req.dst && neigh.hardware_addr == req.lladdr)
    );

    let replaced = NeighRequest {
        lladdr: Some([1, 2, 3, 4, 5, 6]),
        ..req
    };
    add_neigh_state(replaced, NLM_F_REPLACE).unwrap();
    assert!(
        SERVICE
            .lock()
            .network_snapshot()
            .neighbors
            .iter()
            .any(|neigh| neigh.dst == replaced.dst && neigh.hardware_addr == replaced.lladdr)
    );
}

#[def_test(serial)]
fn test_add_incomplete_neighbour_without_link_address() {
    init_test_state();
    let req = NeighRequest {
        family: wire_route::FAMILY_IPV4,
        ifindex: 2,
        state: wire_neigh::STATE_INCOMPLETE,
        flags: 0,
        dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 11)),
        lladdr: None,
    };

    add_neigh_state(req, NLM_F_CREATE).unwrap();

    assert!(
        SERVICE
            .lock()
            .network_snapshot()
            .neighbors
            .iter()
            .any(|neigh| neigh.dst == req.dst && neigh.hardware_addr.is_none())
    );
}

#[def_test(serial)]
fn test_reject_unsupported_rtnetlink_mutations() {
    init_test_state();
    let ipv6 = IpAddress::Ipv6(Ipv6Address::UNSPECIFIED);
    let addr = AddrRequest {
        index: 2,
        family: wire_route::FAMILY_IPV6,
        prefix_len: 128,
        scope: wire_route::SCOPE_UNIVERSE,
        address: ipv6,
    };
    assert_eq!(
        add_addr_state(addr, NLM_F_CREATE),
        Err(LinuxError::EAFNOSUPPORT)
    );

    let route = RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        table: 100,
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: ROUTE_TYPE_UNICAST,
        oif: 2,
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(172, 16, 0, 0))),
        gateway: None,
        prefsrc: None,
    };
    assert_eq!(
        add_route_state(route, NLM_F_CREATE),
        Err(LinuxError::EOPNOTSUPP)
    );

    let neigh = NeighRequest {
        family: wire_route::FAMILY_IPV6,
        ifindex: 2,
        state: wire_neigh::STATE_PERMANENT,
        flags: 0,
        dst: ipv6,
        lladdr: Some([1, 2, 3, 4, 5, 6]),
    };
    assert_eq!(
        add_neigh_state(neigh, NLM_F_CREATE),
        Err(LinuxError::EAFNOSUPPORT)
    );
}

#[def_test(serial)]
fn test_reject_extended_unsupported_route_table() {
    init_test_state();
    let mut payload = Vec::new();
    RtMsg {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 0,
        tos: 0,
        table: 0,
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: ROUTE_TYPE_UNICAST,
        flags: 0,
    }
    .write(&mut payload);
    push_attr(
        &mut payload,
        wire_route::attr::DST,
        &Ipv4Address::new(172, 17, 0, 0).octets(),
    );
    push_attr(&mut payload, wire_route::attr::OIF, &2u32.to_ne_bytes());
    push_attr(&mut payload, wire_route::attr::TABLE, &510u32.to_ne_bytes());
    let request = build_nlmsg(
        RTM_NEWROUTE,
        23,
        NLM_F_REQUEST | NLM_F_CREATE | NLM_F_ACK,
        payload,
    );

    let packets = handle_route_request(&request);

    assert_eq!(packets.len(), 1);
    assert_eq!(read_u16_ne(&packets[0].data, 4), Some(NLMSG_ERROR));
    assert_eq!(
        read_i32_ne(&packets[0].data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EOPNOTSUPP.into_raw()))
    );
}

#[def_test(serial)]
fn test_reject_unsupported_neighbour_state_and_flags() {
    init_test_state();
    let req = NeighRequest {
        family: wire_route::FAMILY_IPV4,
        ifindex: 2,
        state: wire_neigh::STATE_PERMANENT,
        flags: 0,
        dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 12)),
        lladdr: None,
    };
    assert_eq!(add_neigh_state(req, NLM_F_CREATE), Err(LinuxError::EINVAL));
    assert_eq!(
        add_neigh_state(
            NeighRequest {
                flags: 1,
                lladdr: Some([1, 2, 3, 4, 5, 6]),
                ..req
            },
            NLM_F_CREATE,
        ),
        Err(LinuxError::EOPNOTSUPP)
    );
}

#[def_test]
fn test_dump_error_omits_done_message() {
    let request = build_nlmsg(RTM_GETLINK, 13, NLM_F_REQUEST, Vec::new());
    let header = NlMsgHeader::read(&request).unwrap();
    let packets = finish_dump(&request, &header, Err(LinuxError::ENODEV));

    assert_eq!(packets.len(), 1);
    assert_eq!(read_u16_ne(&packets[0].data, 4), Some(NLMSG_ERROR));
    assert_eq!(
        read_i32_ne(&packets[0].data, NLMSG_HDR_LEN),
        Some(-(LinuxError::ENODEV.into_raw()))
    );
}

#[def_test(serial)]
fn test_dump_routes_returns_done_message() {
    init_test_state();
    let header = nl_header(RTM_GETROUTE, NLM_F_REQUEST, 7);
    let packets = handle_route_request(&build_nlmsg(
        RTM_GETROUTE,
        7,
        NLM_F_REQUEST,
        vec![wire_route::FAMILY_IPV4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    ));

    assert!(packets.len() >= 2);
    let last = packets.last().unwrap();
    let seq = header.seq;
    assert_eq!(read_u16_ne(&last.data, 4), Some(NLMSG_DONE));
    assert_eq!(seq, 7);
}

#[def_test(serial)]
fn test_ack_response_for_newaddr() {
    init_test_state();
    let mut payload = Vec::new();
    push_u8(&mut payload, wire_route::FAMILY_IPV4);
    push_u8(&mut payload, 24);
    push_u8(&mut payload, 0);
    push_u8(&mut payload, wire_route::SCOPE_UNIVERSE);
    push_u32_ne(&mut payload, 2);
    payload.extend(attr(wire_addr::attr::LOCAL, &[192, 168, 1, 77]));

    let request = build_nlmsg(
        RTM_NEWADDR,
        11,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
        payload,
    );
    let packets = handle_route_request(&request);

    assert_eq!(packets.len(), 1);
    assert_eq!(read_u16_ne(&packets[0].data, 4), Some(NLMSG_ERROR));
    assert_eq!(read_i32_ne(&packets[0].data, NLMSG_HDR_LEN), Some(0));
}

#[def_test(serial)]
fn test_publish_kobject_uevent_to_group_subscriber() {
    let socket = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 }))
        .unwrap();

    let payload = b"add@/devices/test\0ACTION=add\0SUBSYSTEM=block\0DEVPATH=/devices/test\0";
    publish_kobject_uevent(1, payload);

    let packet = socket.inner.rx_queue.lock().pop_front().unwrap();
    assert_eq!(packet.from.groups, 1);
    assert!(packet.data.starts_with(payload));
    assert!(packet.data[payload.len()..].starts_with(b"SEQNUM="));
    assert_eq!(packet.data.last(), Some(&0));
}

#[def_test(serial)]
fn test_publish_kobject_uevent_drops_when_rx_queue_limit_exceeded() {
    let socket = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 }))
        .unwrap();

    let next_seqnum = UEVENT_SEQNUM.load(Ordering::Relaxed) + 1;
    let seqnum_suffix_len = format!("SEQNUM={next_seqnum}\0").len();
    let payload = vec![0u8; NETLINK_RX_QUEUE_LIMIT - seqnum_suffix_len];
    publish_kobject_uevent(1, &payload);
    publish_kobject_uevent(1, b"overflow");

    let mut queue = socket.inner.rx_queue.lock();
    let packet = queue.pop_front().unwrap();
    assert!(packet.data.starts_with(&payload));
    assert!(queue.pop_front().is_none());
}

#[def_test(serial)]
fn test_publish_kobject_uevent_appends_monotonic_seqnum() {
    let socket = NetlinkSocket::new(NETLINK_KOBJECT_UEVENT);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 0, groups: 1 }))
        .unwrap();

    publish_kobject_uevent(1, b"first\0");
    publish_kobject_uevent(1, b"second\0");

    let mut queue = socket.inner.rx_queue.lock();
    let first = queue.pop_front().unwrap();
    let second = queue.pop_front().unwrap();

    let first_seq = ::core::str::from_utf8(&first.data)
        .unwrap()
        .split('\0')
        .find_map(|entry| entry.strip_prefix("SEQNUM="))
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let second_seq = ::core::str::from_utf8(&second.data)
        .unwrap()
        .split('\0')
        .find_map(|entry| entry.strip_prefix("SEQNUM="))
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(second_seq > first_seq);
}

#[def_test(serial)]
fn test_recv_preserves_packet_when_dst_buffer_too_small() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr { pid: 1, groups: 0 },
        data: vec![1, 2, 3, 4],
    });

    let mut buf = [0u8; 2];
    let result = socket.recv(Cursor::new(buf.as_mut_slice()), RecvOptions::default());

    assert_eq!(result, Err(LinuxError::EMSGSIZE.into()));

    let mut queue = socket.inner.rx_queue.lock();
    let packet = queue.pop_front().unwrap();
    assert_eq!(packet.data, vec![1, 2, 3, 4]);
    assert!(queue.pop_front().is_none());
}
