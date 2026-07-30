// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::{boxed::Box, format, string::String, sync::Arc, vec, vec::Vec};

use ::core::{sync::atomic::Ordering, task::Waker};
use kcred::Cred;
use kerrno::LinuxError;
use khal::time::TimeValue;
use kio::Cursor;
use unittest::def_test;

use super::{
    rtnetlink::*,
    socket::publish_kobject_uevent,
    wire::{
        IfAddrMsg, IfInfoMsg, NLMSG_DONE, NLMSG_ERROR, NlMsgHeader, RtMsg, addr as wire_addr,
        build_nlmsg, ip_addr_bytes, link as wire_link, neigh as wire_neigh, parse_attrs, push_attr,
        push_i32_ne, push_u8, push_u16_ne, push_u32_ne, read_i32_ne, read_u16_ne,
        route as wire_route,
    },
    *,
};
use crate::{
    RecvOptions, SERVICE, SendOptions, SocketAddrEx,
    buf::PacketBuf,
    device::{
        LINK_FLAG_BROADCAST, LINK_FLAG_LOOPBACK, LINK_FLAG_LOWER_UP, LINK_FLAG_MULTICAST,
        LINK_FLAG_RUNNING, LINK_FLAG_UP, LinkConfigUpdate, LinkKind, LinkSnapshot, LoopbackDevice,
        NeighborState, NeighborUpdate, NetDevice,
    },
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address},
    router::{
        Ipv4AddrEntry, ROUTE_PROTOCOL_BOOT, ROUTE_PROTOCOL_KERNEL, ROUTE_SCOPE_HOST,
        ROUTE_SCOPE_LINK, ROUTE_SCOPE_UNIVERSE, ROUTE_TABLE_MAIN, ROUTE_TYPE_UNICAST, RouteAttrs,
        Router, Rule,
    },
    service::Service,
    socket::SocketOps,
};

fn ifindex(index: u32) -> InterfaceIndex {
    InterfaceIndex::new(index).unwrap()
}

fn update_link_state(request: LinkUpdateRequest) -> Result<(), LinuxError> {
    request.apply()
}

fn add_addr_state(request: AddrRequest, flags: u16) -> Result<(), LinuxError> {
    request.add(RequestFlags::from_raw(flags))
}

fn del_addr_state(request: AddrRequest) -> Result<(), LinuxError> {
    request.delete()
}

fn add_route_state(request: RouteRequest, flags: u16) -> Result<(), LinuxError> {
    request.add(RequestFlags::from_raw(flags))
}

fn del_route_state(request: RouteRequest) -> Result<(), LinuxError> {
    request.delete()
}

fn add_neigh_state(request: NeighRequest, flags: u16) -> Result<(), LinuxError> {
    request.add(RequestFlags::from_raw(flags))
}

struct TestDevice {
    device_id: Option<kdevice::DeviceId>,
    link: LinkSnapshot,
    ipv4_addrs: Arc<ksync::Mutex<Vec<Ipv4Cidr>>>,
    local_ipv4_addrs: Arc<ksync::Mutex<Vec<Ipv4Cidr>>>,
    neighbors: Arc<ksync::Mutex<Vec<NeighborUpdate>>>,
}

impl TestDevice {
    fn new(link: LinkSnapshot, ipv4_addr: Option<Ipv4Cidr>) -> Self {
        Self {
            device_id: None,
            link,
            ipv4_addrs: Arc::new(ksync::Mutex::new(ipv4_addr.into_iter().collect())),
            local_ipv4_addrs: Arc::new(ksync::Mutex::new(Vec::new())),
            neighbors: Arc::new(ksync::Mutex::new(Vec::new())),
        }
    }

    fn with_device_id(mut self, device_id: kdevice::DeviceId) -> Self {
        self.device_id = Some(device_id);
        self
    }
}

struct TestNetworkState {
    eth0_ipv4_addrs: Arc<ksync::Mutex<Vec<Ipv4Cidr>>>,
    eth0_neighbors: Arc<ksync::Mutex<Vec<NeighborUpdate>>>,
}

impl NetDevice for TestDevice {
    fn name(&self) -> &str {
        &self.link.name
    }

    fn link_kind(&self) -> LinkKind {
        self.link.kind
    }

    fn mtu(&self) -> usize {
        self.link.mtu
    }

    fn is_link_up(&self) -> bool {
        self.link.flags & LINK_FLAG_UP != 0
    }

    fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot {
        LinkSnapshot {
            ifindex,
            ..self.link.clone()
        }
    }

    fn device_id(&self) -> Option<kdevice::DeviceId> {
        self.device_id
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

    fn register_rx_waker(
        &self,
        _source_waker: &Waker,
        _context: &mut kpoll::PollContext<'_>,
    ) -> Result<(), kpoll::PollRegisterError> {
        Ok(())
    }

    fn set_name(&mut self, name: String) {
        self.link.name = name;
    }

    fn set_mtu(&mut self, mtu: usize) -> Result<(), LinuxError> {
        self.link.mtu = mtu;
        Ok(())
    }

    fn set_ipv4_addrs(&mut self, assigned_addrs: &[Ipv4Cidr], local_addrs: &[Ipv4Cidr]) {
        *self.ipv4_addrs.lock() = assigned_addrs.to_vec();
        *self.local_ipv4_addrs.lock() = local_addrs.to_vec();
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

    fn apply_neighbor_update(&mut self, update: NeighborUpdate) -> Result<(), LinuxError> {
        let mut neighbors = self.neighbors.lock();
        if let Some(existing) = neighbors
            .iter_mut()
            .find(|neighbor| neighbor.dst == update.dst)
        {
            *existing = update;
        } else {
            neighbors.push(update);
        }
        Ok(())
    }

    fn has_neighbor(&self, dst: IpAddress) -> bool {
        self.neighbors
            .lock()
            .iter()
            .any(|neighbor| neighbor.dst == dst)
    }
}

fn init_test_state() -> TestNetworkState {
    let mut router = Router::new();
    let lo_ip = Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8);
    let lo_dev = router.add_device(Box::new(TestDevice::new(
        LinkSnapshot {
            ifindex: 1,
            name: String::from("lo"),
            flags: LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOOPBACK | LINK_FLAG_LOWER_UP,
            mtu: 65_536,
            operstate: 0,
            kind: LinkKind::Loopback,
            hardware_addr: [0; 6],
            broadcast_addr: [0; 6],
        },
        Some(lo_ip),
    )));
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev: lo_dev,
            addr: lo_ip,
            scope: ROUTE_SCOPE_HOST,
        })
        .unwrap();

    let eth0_ip = Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 2), 24);
    let eth0 = TestDevice::new(
        LinkSnapshot {
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
        },
        Some(eth0_ip),
    );
    let state = TestNetworkState {
        eth0_ipv4_addrs: eth0.ipv4_addrs.clone(),
        eth0_neighbors: eth0.neighbors.clone(),
    };
    let eth0_dev = router.add_device(Box::new(eth0));
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev: eth0_dev,
            addr: eth0_ip,
            scope: ROUTE_SCOPE_UNIVERSE,
        })
        .unwrap();
    router
        .add_route_rule(Rule::with_route_attrs(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1))),
            eth0_dev,
            route_attrs(ROUTE_SCOPE_UNIVERSE, None),
        ))
        .unwrap();

    if SERVICE.is_inited() {
        SERVICE.lock().replace_router_for_tests(router);
    } else {
        SERVICE.init_once(ksync::Mutex::new(Service::new(router)));
    }
    state
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

fn build_ipv4_addr_mutation(address: Ipv4Address, seq: u32, flags: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    IfAddrMsg {
        family: wire_route::FAMILY_IPV4,
        prefix_len: 24,
        flags: 0,
        scope: wire_route::SCOPE_UNIVERSE,
        index: 2,
    }
    .write(&mut payload);
    push_attr(&mut payload, wire_addr::attr::LOCAL, &address.octets());
    build_nlmsg(RTM_NEWADDR, seq, flags, payload)
}

fn ipv4_route_request(dst: Ipv4Address, gateway: Option<Ipv4Address>) -> RouteRequest {
    RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 0,
        table: u32::from(ROUTE_TABLE_MAIN),
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: ROUTE_TYPE_UNICAST,
        oif: Some(ifindex(2)),
        dst: Some(IpAddress::Ipv4(dst)),
        src: None,
        gateway: gateway.map(IpAddress::Ipv4),
        prefsrc: None,
    }
}

#[def_test]
fn test_interface_index_rejects_zero() {
    assert_eq!(InterfaceIndex::new(0), Err(LinuxError::ENODEV));
}

#[def_test(serial)]
fn test_owner_state_contains_links_addresses_and_routes() {
    init_test_state();
    let service = SERVICE.lock();
    let links = service.link_snapshots();
    let addrs = service.ipv4_addr_snapshots();
    let routes = service.route_snapshot();

    assert!(service.is_ipv4_broadcast(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 255))));
    assert!(
        service.is_ipv4_bindable_broadcast(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 255)))
    );
    assert!(!service.is_ipv4_broadcast(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2))));
    assert!(!service.is_ipv4_broadcast(IpAddress::Ipv4(Ipv4Address::new(192, 168, 2, 255))));
    assert_eq!(links.len(), 2);
    assert_eq!(addrs.len(), 2);
    assert_eq!(links[0].name, "lo");
    assert_eq!(links[1].name, "eth0");
    assert!(routes.iter().any(|route| {
        route.filter == IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 0), 8))
            && route.scope == wire_route::SCOPE_HOST
            && route.via.is_none()
    }));
    assert!(routes.iter().any(|route| {
        route.filter == IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 2), 32))
            && route.scope == wire_route::SCOPE_HOST
            && route.via.is_none()
    }));
    assert!(routes.iter().any(|route| {
        route.filter.prefix_len() == 0
            && route.scope == wire_route::SCOPE_UNIVERSE
            && route.via == Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1)))
    }));
}

#[def_test(serial)]
fn test_link_dump_omits_unmodelled_lower_link() {
    init_test_state();
    let request = build_nlmsg(RTM_GETLINK, 2, NLM_F_REQUEST, Vec::new());
    let packets = handle_rtnetlink_request(&request);
    let eth0 = packets
        .iter()
        .find(|packet| {
            let payload = &packet.data[NLMSG_HDR_LEN..];
            IfInfoMsg::read(payload).is_ok_and(|info| info.index == 2)
        })
        .expect("eth0 link dump");
    let payload = &eth0.data[NLMSG_HDR_LEN..];
    let info = IfInfoMsg::read(payload).unwrap();
    let family = info.family;
    let change = info.change;
    assert_eq!(family, 0);
    assert_eq!(change, 0);
    assert!(
        parse_attrs(&payload[IfInfoMsg::SIZE..])
            .unwrap()
            .iter()
            .all(|attr| attr.kind != wire_link::attr::LINK)
    );
}

#[def_test(serial)]
fn test_route_without_preferred_source_selects_gateway_subnet_address() {
    init_test_state();
    let address = Ipv4Address::new(10, 0, 0, 2);
    add_addr_state(
        AddrRequest {
            index: ifindex(2),
            family: wire_route::FAMILY_IPV4,
            prefix_len: 24,
            scope: wire_route::SCOPE_UNIVERSE,
            address: IpAddress::Ipv4(address),
        },
        NLM_F_CREATE | NLM_F_EXCL,
    )
    .unwrap();

    let destination = Ipv4Address::new(172, 26, 0, 0);
    add_route_state(
        ipv4_route_request(destination, Some(Ipv4Address::new(10, 0, 0, 1))),
        NLM_F_CREATE | NLM_F_EXCL,
    )
    .unwrap();

    let destination =
        smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(172, 26, 0, 7));
    assert_eq!(
        SERVICE
            .lock()
            .get_smoltcp_source_address(&destination)
            .unwrap(),
        smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(10, 0, 0, 2))
    );
}

#[def_test(serial)]
fn test_unicast_egress_mtu_comes_from_selected_device() {
    init_test_state();
    update_link_state(LinkUpdateRequest {
        index: ifindex(1),
        flags: 0,
        change: 0,
        name: None,
        mtu: Some(68),
    })
    .unwrap();

    let external = smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(8, 8, 8, 8));
    let loopback = smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(127, 0, 0, 1));
    assert_eq!(SERVICE.lock().egress_mtu(&external), Ok(1500));
    assert_eq!(SERVICE.lock().egress_mtu(&loopback), Ok(68));
}

#[def_test(serial)]
fn test_remove_device_cleans_owner_state_and_reindexes_remaining_device() {
    let loopback_id = kdevice::DeviceId::new(100);
    let removed_id = kdevice::DeviceId::new(101);
    let remaining_id = kdevice::DeviceId::new(102);
    let mut router = Router::new();

    let loopback_addr = Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8);
    let loopback_dev = router.add_device(Box::new(
        TestDevice::new(
            LinkSnapshot {
                ifindex: 1,
                name: String::from("lo"),
                flags: LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOOPBACK | LINK_FLAG_LOWER_UP,
                mtu: 65_536,
                operstate: 0,
                kind: LinkKind::Loopback,
                hardware_addr: [0; 6],
                broadcast_addr: [0; 6],
            },
            Some(loopback_addr),
        )
        .with_device_id(loopback_id),
    ));
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev: loopback_dev,
            addr: loopback_addr,
            scope: ROUTE_SCOPE_HOST,
        })
        .unwrap();

    let removed_addr = Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 2), 24);
    let removed_dev = router.add_device(Box::new(
        TestDevice::new(
            LinkSnapshot {
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
                hardware_addr: [0x02, 0, 0, 0, 0, 2],
                broadcast_addr: [0xff; 6],
            },
            Some(removed_addr),
        )
        .with_device_id(removed_id),
    ));
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev: removed_dev,
            addr: removed_addr,
            scope: ROUTE_SCOPE_UNIVERSE,
        })
        .unwrap();

    let remaining_addr = Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 2), 24);
    let remaining_dev = router.add_device(Box::new(
        TestDevice::new(
            LinkSnapshot {
                ifindex: 3,
                name: String::from("eth1"),
                flags: LINK_FLAG_UP
                    | LINK_FLAG_RUNNING
                    | LINK_FLAG_BROADCAST
                    | LINK_FLAG_MULTICAST
                    | LINK_FLAG_LOWER_UP,
                mtu: 1400,
                operstate: 6,
                kind: LinkKind::Ethernet,
                hardware_addr: [0x02, 0, 0, 0, 0, 3],
                broadcast_addr: [0xff; 6],
            },
            Some(remaining_addr),
        )
        .with_device_id(remaining_id),
    ));
    router
        .add_ipv4_addr(Ipv4AddrEntry {
            dev: remaining_dev,
            addr: remaining_addr,
            scope: ROUTE_SCOPE_UNIVERSE,
        })
        .unwrap();

    let removed_route = Ipv4Cidr::new(Ipv4Address::new(172, 27, 0, 0), 24);
    router
        .add_route_rule(Rule::with_route_attrs(
            removed_route.into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1))),
            removed_dev,
            route_attrs(ROUTE_SCOPE_UNIVERSE, None),
        ))
        .unwrap();
    let remaining_route = Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 0), 24);
    router
        .add_route_rule(Rule::with_route_attrs(
            remaining_route.into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 1))),
            remaining_dev,
            route_attrs(ROUTE_SCOPE_UNIVERSE, None),
        ))
        .unwrap();

    if SERVICE.is_inited() {
        SERVICE.lock().replace_router_for_tests(router);
    } else {
        SERVICE.init_once(ksync::Mutex::new(Service::new(router)));
    }
    assert!(SERVICE.lock().remove_device_by_model_id(removed_id));

    let service = SERVICE.lock();
    let links = service.link_snapshots();
    assert_eq!(links.len(), 2);
    assert_eq!((links[0].ifindex, links[0].name.as_str()), (1, "lo"));
    assert_eq!((links[1].ifindex, links[1].name.as_str()), (2, "eth1"));

    let addrs = service.ipv4_addr_snapshots();
    assert!(
        !addrs
            .iter()
            .any(|snapshot| snapshot.entry.addr == removed_addr)
    );
    assert!(addrs.iter().any(|snapshot| {
        snapshot.entry.addr == remaining_addr && snapshot.entry.dev == 1 && snapshot.label == "eth1"
    }));

    let routes = service.route_snapshot();
    assert!(
        !routes
            .iter()
            .any(|route| route.filter == removed_route.into())
    );
    assert!(routes.iter().any(|route| {
        route.is_configured() && route.filter == remaining_route.into() && route.dev == 1
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

    let request = LinkUpdateRequest::decode(&payload).unwrap();
    assert_eq!(request.index, ifindex(2));
    assert_eq!(request.name, Some(String::from("eth1")));
    assert_eq!(request.mtu, Some(1400));
}

#[def_test(serial)]
fn test_update_link_state_rename_and_down() {
    init_test_state();
    let request = LinkUpdateRequest {
        index: ifindex(2),
        flags: 0,
        change: LINK_FLAG_UP,
        name: Some(String::from("ens3")),
        mtu: Some(1400),
    };
    update_link_state(request).unwrap();

    let links = SERVICE.lock().link_snapshots();
    let link = links.iter().find(|link| link.ifindex == 2).unwrap();
    assert_eq!(link.name, "ens3");
    assert_eq!(link.mtu, 1400);
    assert_eq!(link.flags & LINK_FLAG_UP, 0);
    assert!(
        SERVICE
            .lock()
            .is_ipv4_broadcast(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 255)))
    );
    assert!(
        !SERVICE
            .lock()
            .is_ipv4_bindable_broadcast(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 255)))
    );
    assert!(
        SERVICE
            .lock()
            .ipv4_addr_snapshots()
            .iter()
            .any(|snapshot| snapshot.entry.dev == 1 && snapshot.label == "ens3")
    );
    let destination = smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(8, 8, 8, 8));
    assert_eq!(
        SERVICE.lock().egress_mtu(&destination),
        Err(LinuxError::ENETDOWN.into())
    );
    assert_eq!(
        SERVICE.lock().get_smoltcp_source_address(&destination),
        Err(LinuxError::ENETUNREACH.into())
    );
}

#[def_test(serial)]
fn test_reject_invalid_link_name_and_mtu_without_partial_update() {
    init_test_state();

    for name in ["", ".", "..", "eth/1", "eth:1", "eth 1", "1234567890123456"] {
        assert_eq!(
            update_link_state(LinkUpdateRequest {
                index: ifindex(2),
                flags: 0,
                change: 0,
                name: Some(String::from(name)),
                mtu: None,
            }),
            Err(LinuxError::EINVAL)
        );
    }
    for mtu in [67, 1501, 65_536] {
        assert_eq!(
            update_link_state(LinkUpdateRequest {
                index: ifindex(2),
                flags: 0,
                change: 0,
                name: None,
                mtu: Some(mtu),
            }),
            Err(LinuxError::EINVAL)
        );
    }
    assert_eq!(
        SERVICE.lock().update_device_link(
            1,
            LinkConfigUpdate {
                name: Some(String::from("ens3")),
                mtu: Some(1501),
                is_up: Some(false),
            },
        ),
        Err(LinuxError::EINVAL)
    );

    let link = SERVICE
        .lock()
        .link_snapshot_for_ifindex(2)
        .expect("eth0 remains present");
    assert_eq!(link.name, "eth0");
    assert_eq!(link.mtu, 1500);
    assert_ne!(link.flags & LINK_FLAG_UP, 0);
}

#[def_test(serial)]
fn test_add_replace_and_delete_address() {
    let state = init_test_state();
    let req = AddrRequest {
        index: ifindex(2),
        family: wire_route::FAMILY_IPV4,
        prefix_len: 24,
        scope: wire_route::SCOPE_UNIVERSE,
        address: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 99)),
    };

    add_addr_state(req, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert_eq!(
        *state.eth0_ipv4_addrs.lock(),
        vec![
            Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 2), 24),
            Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 99), 24),
        ]
    );
    assert!(SERVICE.lock().ipv4_addr_snapshots().iter().any(|snapshot| {
        IpAddress::Ipv4(snapshot.entry.addr.address()) == req.address && snapshot.entry.dev == 1
    }));

    let replaced = AddrRequest {
        scope: wire_route::SCOPE_HOST,
        ..req
    };
    add_addr_state(replaced, NLM_F_REPLACE).unwrap();
    assert!(SERVICE.lock().ipv4_addr_snapshots().iter().any(|snapshot| {
        IpAddress::Ipv4(snapshot.entry.addr.address()) == replaced.address
            && snapshot.entry.scope == wire_route::SCOPE_UNIVERSE
    }));

    del_addr_state(replaced).unwrap();
    assert!(
        !SERVICE.lock().ipv4_addr_snapshots().iter().any(|snapshot| {
            IpAddress::Ipv4(snapshot.entry.addr.address()) == replaced.address
                && snapshot.entry.dev == 1
        })
    );
}

#[def_test(serial)]
fn test_secondary_address_takes_over_routes_and_source() {
    let state = init_test_state();
    let secondary = AddrRequest {
        index: ifindex(2),
        family: wire_route::FAMILY_IPV4,
        prefix_len: 24,
        scope: wire_route::SCOPE_UNIVERSE,
        address: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 99)),
    };
    add_addr_state(secondary, NLM_F_CREATE | NLM_F_EXCL).unwrap();

    let explicit_route = RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 0,
        table: u32::from(ROUTE_TABLE_MAIN),
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: ROUTE_TYPE_UNICAST,
        oif: Some(ifindex(2)),
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(172, 20, 0, 0))),
        src: None,
        gateway: None,
        prefsrc: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2))),
    };
    add_route_state(explicit_route, NLM_F_CREATE | NLM_F_EXCL).unwrap();

    let primary = AddrRequest {
        address: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2)),
        ..secondary
    };
    del_addr_state(primary).unwrap();

    assert_eq!(
        *state.eth0_ipv4_addrs.lock(),
        vec![Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 99), 24)]
    );
    let destination = smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(8, 8, 8, 8));
    assert_eq!(
        SERVICE
            .lock()
            .get_smoltcp_source_address(&destination)
            .unwrap(),
        smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(192, 168, 1, 99))
    );

    let routes = SERVICE.lock().route_snapshot();
    let connected = routes
        .iter()
        .filter(|route| {
            route.filter == IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 0), 24))
                && route.dev == 1
        })
        .collect::<Vec<_>>();
    assert_eq!(connected.len(), 1);
    assert_eq!(
        connected[0].preferred_source(),
        Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 99)))
    );
    assert!(!routes.iter().any(|route| {
        route.preferred_source() == Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2)))
    }));
    assert!(!routes.iter().any(|route| {
        route.filter == IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(172, 20, 0, 0), 24))
    }));
}

#[def_test(serial)]
fn test_ipv4_address_capacity_is_enforced_without_partial_update() {
    let state = init_test_state();
    for host in 10..16 {
        add_addr_state(
            AddrRequest {
                index: ifindex(2),
                family: wire_route::FAMILY_IPV4,
                prefix_len: 24,
                scope: wire_route::SCOPE_UNIVERSE,
                address: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, host)),
            },
            NLM_F_CREATE | NLM_F_EXCL,
        )
        .unwrap();
    }

    assert_eq!(SERVICE.lock().iface.ip_addrs().len(), 8);
    assert_eq!(state.eth0_ipv4_addrs.lock().len(), 7);
    add_addr_state(
        AddrRequest {
            index: ifindex(2),
            family: wire_route::FAMILY_IPV4,
            prefix_len: 24,
            scope: wire_route::SCOPE_HOST,
            address: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 10)),
        },
        NLM_F_REPLACE,
    )
    .unwrap();
    assert_eq!(SERVICE.lock().iface.ip_addrs().len(), 8);
    assert_eq!(
        add_addr_state(
            AddrRequest {
                index: ifindex(2),
                family: wire_route::FAMILY_IPV4,
                prefix_len: 24,
                scope: wire_route::SCOPE_UNIVERSE,
                address: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 16)),
            },
            NLM_F_CREATE | NLM_F_EXCL,
        ),
        Err(LinuxError::ENOBUFS)
    );
    assert_eq!(SERVICE.lock().iface.ip_addrs().len(), 8);
    assert_eq!(state.eth0_ipv4_addrs.lock().len(), 7);
}

#[def_test(serial)]
fn test_add_and_delete_route() {
    init_test_state();
    let req = RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 0,
        table: u32::from(ROUTE_TABLE_MAIN),
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: ROUTE_TYPE_UNICAST,
        oif: Some(ifindex(2)),
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(172, 16, 0, 0))),
        src: None,
        gateway: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 1))),
        prefsrc: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2))),
    };

    add_route_state(req, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert!(
        SERVICE
            .lock()
            .route_snapshot()
            .iter()
            .any(|route| route.filter.address() == req.dst.unwrap() && route.via == req.gateway)
    );

    del_route_state(req).unwrap();
    assert!(
        !SERVICE
            .lock()
            .route_snapshot()
            .iter()
            .any(|route| route.filter.address() == req.dst.unwrap() && route.via == req.gateway)
    );
}

#[def_test(serial)]
fn test_route_without_dst_attribute_uses_zero_prefix() {
    init_test_state();
    let request = RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 0,
        table: u32::from(ROUTE_TABLE_MAIN),
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: ROUTE_TYPE_UNICAST,
        oif: Some(ifindex(2)),
        dst: None,
        src: None,
        gateway: None,
        prefsrc: None,
    };
    add_route_state(request, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert!(SERVICE.lock().route_snapshot().iter().any(|route| {
        route.is_configured()
            && route.filter
                == IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, request.dst_len))
    }));

    del_route_state(RouteRequest {
        protocol: 0,
        scope: wire_route::SCOPE_NOWHERE,
        route_type: 0,
        oif: None,
        ..request
    })
    .unwrap();
    assert!(!SERVICE.lock().route_snapshot().iter().any(|route| {
        route.is_configured()
            && route.filter
                == IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, request.dst_len))
    }));
}

#[def_test(serial)]
fn test_address_owned_routes_reject_control_plane_replace_and_delete() {
    init_test_state();
    let request = RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 0,
        table: u32::from(ROUTE_TABLE_MAIN),
        protocol: ROUTE_PROTOCOL_KERNEL,
        scope: ROUTE_SCOPE_LINK,
        route_type: ROUTE_TYPE_UNICAST,
        oif: Some(ifindex(2)),
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 0))),
        src: None,
        gateway: None,
        prefsrc: Some(IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2))),
    };

    assert_eq!(
        add_route_state(request, NLM_F_CREATE | NLM_F_REPLACE),
        Err(LinuxError::EEXIST)
    );
    assert_eq!(del_route_state(request), Err(LinuxError::ESRCH));
    assert!(SERVICE.lock().route_snapshot().iter().any(|route| {
        !route.is_configured()
            && route.filter == IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::new(192, 168, 1, 0), 24))
            && route.protocol == ROUTE_PROTOCOL_KERNEL
    }));
}

#[def_test(serial)]
fn test_route_rejects_nonlocal_preferred_source() {
    init_test_state();
    let request = RouteRequest {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 0,
        table: u32::from(ROUTE_TABLE_MAIN),
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: ROUTE_TYPE_UNICAST,
        oif: Some(ifindex(2)),
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(172, 21, 0, 0))),
        src: None,
        gateway: None,
        prefsrc: Some(IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 10))),
    };

    assert_eq!(
        add_route_state(request, NLM_F_CREATE | NLM_F_EXCL),
        Err(LinuxError::EINVAL)
    );
}

#[def_test(serial)]
fn test_add_and_replace_neighbour() {
    let state = init_test_state();
    let req = NeighRequest {
        family: wire_route::FAMILY_IPV4,
        ifindex: ifindex(2),
        state: wire_neigh::STATE_PERMANENT,
        flags: 0,
        dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 10)),
        lladdr: Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
    };

    add_neigh_state(req, NLM_F_CREATE | NLM_F_REPLACE).unwrap();
    assert!(state.eth0_neighbors.lock().iter().any(|neigh| {
        neigh.dst == req.dst
            && neigh.state
                == NeighborState::Permanent {
                    hardware_addr: req.lladdr.unwrap(),
                }
    }));

    let replaced = NeighRequest {
        lladdr: Some([1, 2, 3, 4, 5, 6]),
        ..req
    };
    add_neigh_state(replaced, NLM_F_REPLACE).unwrap();
    assert!(state.eth0_neighbors.lock().iter().any(|neigh| {
        neigh.dst == replaced.dst
            && neigh.state
                == NeighborState::Permanent {
                    hardware_addr: replaced.lladdr.unwrap(),
                }
    }));
}

#[def_test(serial)]
fn test_add_incomplete_neighbour_without_link_address() {
    let state = init_test_state();
    let req = NeighRequest {
        family: wire_route::FAMILY_IPV4,
        ifindex: ifindex(2),
        state: wire_neigh::STATE_INCOMPLETE,
        flags: 0,
        dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 11)),
        lladdr: None,
    };

    add_neigh_state(req, NLM_F_CREATE).unwrap();

    assert!(
        state
            .eth0_neighbors
            .lock()
            .iter()
            .any(|neigh| neigh.dst == req.dst && neigh.state == NeighborState::Incomplete)
    );
}

#[def_test(serial)]
fn test_loopback_rejects_neighbor_mutation() {
    let mut router = Router::new();
    router.add_device(Box::new(LoopbackDevice::new()));
    if SERVICE.is_inited() {
        SERVICE.lock().replace_router_for_tests(router);
    } else {
        SERVICE.init_once(ksync::Mutex::new(Service::new(router)));
    }
    let req = NeighRequest {
        family: wire_route::FAMILY_IPV4,
        ifindex: ifindex(1),
        state: wire_neigh::STATE_PERMANENT,
        flags: 0,
        dst: IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 2)),
        lladdr: Some([1, 2, 3, 4, 5, 6]),
    };

    assert_eq!(
        add_neigh_state(req, NLM_F_CREATE),
        Err(LinuxError::EOPNOTSUPP)
    );
}

#[def_test]
fn test_loopback_link_down_blocks_ip_send() {
    let mut loopback = LoopbackDevice::new();
    loopback.set_link_up(false);
    let packet = PacketBuf::from_ip_packet_vec(
        1,
        vec![
            0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 127, 0, 0, 1, 127, 0, 0, 1,
        ],
        crate::buf::PacketOwner::DeviceTx,
    );

    assert!(!loopback.send_ip_packet(
        1,
        IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
        packet,
        TimeValue::ZERO,
    ));
}

#[def_test(serial)]
fn test_reject_unsupported_rtnetlink_mutations() {
    init_test_state();
    let ipv6 = IpAddress::Ipv6(Ipv6Address::UNSPECIFIED);
    let addr = AddrRequest {
        index: ifindex(2),
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
        src_len: 0,
        table: 100,
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: ROUTE_TYPE_UNICAST,
        oif: Some(ifindex(2)),
        dst: Some(IpAddress::Ipv4(Ipv4Address::new(172, 16, 0, 0))),
        src: None,
        gateway: None,
        prefsrc: None,
    };
    assert_eq!(
        add_route_state(route, NLM_F_CREATE),
        Err(LinuxError::EOPNOTSUPP)
    );

    let neigh = NeighRequest {
        family: wire_route::FAMILY_IPV6,
        ifindex: ifindex(2),
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
fn test_reject_ipv4_prefix_lengths_above_32() {
    init_test_state();

    let mut addr_payload = Vec::new();
    IfAddrMsg {
        family: wire_route::FAMILY_IPV4,
        prefix_len: 33,
        flags: 0,
        scope: wire_route::SCOPE_UNIVERSE,
        index: 2,
    }
    .write(&mut addr_payload);
    addr_payload.extend(attr(wire_addr::attr::LOCAL, &[192, 168, 1, 77]));
    let addr_request = build_nlmsg(
        RTM_NEWADDR,
        31,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
        addr_payload,
    );
    let addr_packets = handle_rtnetlink_request(&addr_request);
    assert_eq!(
        read_i32_ne(&addr_packets[0].data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EINVAL.into_raw()))
    );

    let mut route_payload = Vec::new();
    RtMsg {
        family: wire_route::FAMILY_IPV4,
        dst_len: 33,
        src_len: 0,
        tos: 0,
        table: wire_route::TABLE_MAIN,
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: wire_route::TYPE_UNICAST,
        flags: 0,
    }
    .write(&mut route_payload);
    push_attr(
        &mut route_payload,
        wire_route::attr::DST,
        &Ipv4Address::new(172, 18, 0, 0).octets(),
    );
    push_attr(
        &mut route_payload,
        wire_route::attr::OIF,
        &2u32.to_ne_bytes(),
    );
    let route_request = build_nlmsg(
        RTM_NEWROUTE,
        32,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
        route_payload,
    );
    let route_packets = handle_rtnetlink_request(&route_request);
    assert_eq!(
        read_i32_ne(&route_packets[0].data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EINVAL.into_raw()))
    );

    for address in [
        Ipv4Address::new(224, 0, 0, 1),
        Ipv4Address::BROADCAST,
        Ipv4Address::UNSPECIFIED,
    ] {
        assert_eq!(
            add_addr_state(
                AddrRequest {
                    index: ifindex(2),
                    family: wire_route::FAMILY_IPV4,
                    prefix_len: 24,
                    scope: wire_route::SCOPE_UNIVERSE,
                    address: IpAddress::Ipv4(address),
                },
                NLM_F_CREATE | NLM_F_EXCL,
            ),
            Err(LinuxError::EINVAL)
        );
    }
    assert_eq!(SERVICE.lock().iface.ip_addrs().len(), 2);
}

#[def_test(serial)]
fn test_rta_src_is_not_treated_as_preferred_source() {
    init_test_state();
    let source = IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 25));
    let preferred_source = IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 2));
    let mut payload = Vec::new();
    RtMsg {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 24,
        tos: 0,
        table: wire_route::TABLE_MAIN,
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: wire_route::TYPE_UNICAST,
        flags: 0,
    }
    .write(&mut payload);
    push_attr(
        &mut payload,
        wire_route::attr::DST,
        &Ipv4Address::new(172, 19, 0, 0).octets(),
    );
    push_attr(&mut payload, wire_route::attr::OIF, &2u32.to_ne_bytes());
    push_attr(&mut payload, wire_route::attr::SRC, &ip_addr_bytes(source));
    push_attr(
        &mut payload,
        wire_route::attr::PREFSRC,
        &ip_addr_bytes(preferred_source),
    );

    let request = RouteRequest::decode(&payload).unwrap();
    assert_eq!(request.src_len, 24);
    assert_eq!(request.src, Some(source));
    assert_eq!(request.prefsrc, Some(preferred_source));
    assert_eq!(
        add_route_state(request, NLM_F_CREATE),
        Err(LinuxError::EOPNOTSUPP)
    );

    assert_eq!(
        add_route_state(
            RouteRequest {
                src_len: 0,
                ..request
            },
            NLM_F_CREATE
        ),
        Err(LinuxError::EINVAL)
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

    let packets = handle_rtnetlink_request(&request);

    assert_eq!(packets.len(), 1);
    assert_eq!(read_u16_ne(&packets[0].data, 4), Some(NLMSG_ERROR));
    assert_eq!(
        read_i32_ne(&packets[0].data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EOPNOTSUPP.into_raw()))
    );

    let request = RouteRequest::decode(
        &request[NLMSG_HDR_LEN..NlMsgHeader::read(&request).unwrap().len as usize],
    )
    .unwrap();
    assert_eq!(del_route_state(request), Err(LinuxError::EOPNOTSUPP));
}

#[def_test(serial)]
fn test_delete_route_rejects_unsupported_type_and_invalid_oif() {
    init_test_state();
    let request = ipv4_route_request(Ipv4Address::new(172, 17, 0, 0), None);

    assert_eq!(
        del_route_state(RouteRequest {
            route_type: 6,
            ..request
        }),
        Err(LinuxError::EOPNOTSUPP)
    );
    assert_eq!(
        del_route_state(RouteRequest {
            oif: Some(ifindex(99)),
            ..request
        }),
        Err(LinuxError::ENODEV)
    );
}

#[def_test(serial)]
fn test_reject_unsupported_neighbour_state_and_flags() {
    init_test_state();
    let req = NeighRequest {
        family: wire_route::FAMILY_IPV4,
        ifindex: ifindex(2),
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
    let packets = handle_rtnetlink_request(&build_nlmsg(
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
    let packets = handle_rtnetlink_request(&request);

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

#[def_test(serial)]
fn test_route_mutation_checks_each_send_credential() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 42, groups: 0 }))
        .unwrap();
    let mutation = build_nlmsg(RTM_NEWLINK, 41, NLM_F_REQUEST | NLM_F_ACK, Vec::new());
    let unprivileged = Cred::new(1000, 1000);
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(mutation.as_slice()),
            SendOptions::default(),
            &unprivileged,
        ),
        Ok(mutation.len())
    );
    let permission_error = socket.inner.rx_queue.lock().pop_front().unwrap();
    assert_eq!(
        read_i32_ne(&permission_error.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EPERM.into_raw()))
    );

    let query = build_nlmsg(RTM_GETLINK, 42, NLM_F_REQUEST, Vec::new());
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(query.as_slice()),
            SendOptions::default(),
            &unprivileged,
        ),
        Ok(query.len())
    );
    while socket.inner.rx_queue.lock().pop_front().is_some() {}
    assert_eq!(
        socket.send_with_cred(
            Cursor::new(mutation.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(mutation.len())
    );
    let invalid_request = socket.inner.rx_queue.lock().pop_front().unwrap();
    assert_eq!(
        read_i32_ne(&invalid_request.data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EINVAL.into_raw()))
    );
}

#[def_test(serial)]
fn test_route_socket_processes_same_class_mutation_batch() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 43, groups: 0 }))
        .unwrap();
    let flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
    let first_addr = Ipv4Address::new(192, 168, 1, 77);
    let second_addr = Ipv4Address::new(192, 168, 1, 78);
    let first = build_ipv4_addr_mutation(first_addr, 51, flags);
    let second = build_ipv4_addr_mutation(second_addr, 52, flags);
    let mut datagram = first;
    datagram.extend_from_slice(&second);

    assert_eq!(
        socket.send_with_cred(
            Cursor::new(datagram.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(datagram.len())
    );

    let mut queue = socket.inner.rx_queue.lock();
    let first_ack = queue.pop_front().unwrap();
    let second_ack = queue.pop_front().unwrap();
    assert_eq!(read_i32_ne(&first_ack.data, NLMSG_HDR_LEN), Some(0));
    assert_eq!(read_i32_ne(&second_ack.data, NLMSG_HDR_LEN), Some(0));
    assert!(queue.pop_front().is_none());
    drop(queue);

    let addrs = SERVICE.lock().ipv4_addr_snapshots();
    assert!(
        addrs
            .iter()
            .any(|snapshot| snapshot.entry.addr.address() == first_addr)
    );
    assert!(
        addrs
            .iter()
            .any(|snapshot| snapshot.entry.addr.address() == second_addr)
    );
}

#[def_test(serial)]
fn test_route_socket_processes_query_batch() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 47, groups: 0 }))
        .unwrap();
    let mut datagram = build_nlmsg(RTM_GETLINK, 57, NLM_F_REQUEST, Vec::new());
    datagram.extend_from_slice(&build_nlmsg(RTM_GETADDR, 58, NLM_F_REQUEST, Vec::new()));

    assert_eq!(
        socket.send_with_cred(
            Cursor::new(datagram.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Ok(datagram.len())
    );
    let mut done_sequences = Vec::new();
    let mut queue = socket.inner.rx_queue.lock();
    while let Some(packet) = queue.pop_front() {
        let header = NlMsgHeader::read(&packet.data).unwrap();
        if header.msg_type == NLMSG_DONE {
            done_sequences.push(header.seq);
        }
    }
    assert_eq!(done_sequences, vec![57, 58]);
}

#[def_test(serial)]
fn test_route_socket_rejects_mixed_query_and_mutation_batch() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 48, groups: 0 }))
        .unwrap();
    let address = Ipv4Address::new(192, 168, 1, 83);
    let mut datagram = build_nlmsg(RTM_GETLINK, 59, NLM_F_REQUEST, Vec::new());
    datagram.extend_from_slice(&build_ipv4_addr_mutation(
        address,
        60,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    ));

    assert_eq!(
        socket.send_with_cred(
            Cursor::new(datagram.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Err(LinuxError::EOPNOTSUPP.into())
    );
    assert!(socket.inner.rx_queue.lock().is_empty());
    assert!(
        !SERVICE
            .lock()
            .ipv4_addr_snapshots()
            .iter()
            .any(|snapshot| snapshot.entry.addr.address() == address)
    );
}

#[def_test(serial)]
fn test_malformed_nlmsg_len_does_not_mutate_owner_state() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 44, groups: 0 }))
        .unwrap();
    let address = Ipv4Address::new(192, 168, 1, 79);
    let mut request = build_ipv4_addr_mutation(
        address,
        53,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    );
    let oversized_len = (request.len() as u32).checked_add(4).unwrap();
    request[..4].copy_from_slice(&oversized_len.to_ne_bytes());

    assert_eq!(
        socket.send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Err(LinuxError::EINVAL.into())
    );
    assert!(socket.inner.rx_queue.lock().is_empty());
    assert!(
        !SERVICE
            .lock()
            .ipv4_addr_snapshots()
            .iter()
            .any(|snapshot| snapshot.entry.addr.address() == address)
    );
}

#[def_test(serial)]
fn test_route_socket_rejects_non_kernel_destination() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 45, groups: 0 }))
        .unwrap();
    let query = build_nlmsg(RTM_GETLINK, 54, NLM_F_REQUEST, Vec::new());

    for destination in [
        NetlinkAddr {
            pid: 1234,
            groups: 0,
        },
        NetlinkAddr { pid: 0, groups: 1 },
    ] {
        assert_eq!(
            socket.send_with_cred(
                Cursor::new(query.as_slice()),
                SendOptions {
                    to: Some(SocketAddrEx::Netlink(destination)),
                    ..SendOptions::default()
                },
                &Cred::root(),
            ),
            Err(LinuxError::EOPNOTSUPP.into())
        );
    }
    assert!(socket.inner.rx_queue.lock().is_empty());
}

#[def_test]
fn test_route_socket_rejects_unsupported_multicast_subscription() {
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    assert_eq!(
        socket.bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 49, groups: 1 })),
        Err(LinuxError::EOPNOTSUPP.into())
    );
    assert!(matches!(
        socket.local_addr(),
        Err(kerrno::KError::NotConnected)
    ));
}

#[def_test(serial)]
fn test_mutation_queue_reservation_failure_preserves_owner_state() {
    init_test_state();
    let socket = NetlinkSocket::new(NETLINK_ROUTE);
    socket
        .bind(SocketAddrEx::Netlink(NetlinkAddr { pid: 46, groups: 0 }))
        .unwrap();
    assert!(socket.inner.rx_queue.lock().push_back(NetlinkPacket {
        from: NetlinkAddr { pid: 0, groups: 0 },
        data: vec![0; NETLINK_RX_QUEUE_LIMIT],
    }));
    let address = Ipv4Address::new(192, 168, 1, 80);
    let request = build_ipv4_addr_mutation(
        address,
        55,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
    );

    assert_eq!(
        socket.send_with_cred(
            Cursor::new(request.as_slice()),
            SendOptions::default(),
            &Cred::root(),
        ),
        Err(LinuxError::ENOBUFS.into())
    );
    assert!(
        !SERVICE
            .lock()
            .ipv4_addr_snapshots()
            .iter()
            .any(|snapshot| snapshot.entry.addr.address() == address)
    );
}

#[def_test(serial)]
fn test_route_replace_updates_gateway_without_leaving_old_rule() {
    init_test_state();
    let destination = Ipv4Address::new(172, 20, 0, 0);
    let original_gateway = Ipv4Address::new(192, 168, 1, 1);
    let replacement_gateway = Ipv4Address::new(192, 168, 1, 254);
    let original = ipv4_route_request(destination, Some(original_gateway));
    add_route_state(original, NLM_F_CREATE | NLM_F_EXCL).unwrap();

    let replacement = ipv4_route_request(destination, Some(replacement_gateway));
    add_route_state(replacement, NLM_F_REPLACE).unwrap();

    let routes = SERVICE.lock().route_snapshot();
    let matching = routes
        .iter()
        .filter(|route| {
            route.is_configured()
                && route.filter == IpCidr::Ipv4(Ipv4Cidr::new(destination, replacement.dst_len))
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].via, Some(IpAddress::Ipv4(replacement_gateway)));
}

#[def_test(serial)]
fn test_route_selector_delete_removes_one_matching_rule() {
    init_test_state();
    let destination = Ipv4Address::new(172, 22, 0, 0);
    {
        let mut service = SERVICE.lock();
        for gateway in [
            Ipv4Address::new(192, 168, 1, 1),
            Ipv4Address::new(192, 168, 1, 254),
        ] {
            service
                .add_route_rule(Rule::with_route_attrs(
                    Ipv4Cidr::new(destination, 24).into(),
                    Some(IpAddress::Ipv4(gateway)),
                    1,
                    route_attrs(ROUTE_SCOPE_UNIVERSE, None),
                ))
                .unwrap();
        }
    }

    let selector = RouteRequest {
        protocol: 0,
        scope: wire_route::SCOPE_NOWHERE,
        route_type: 0,
        oif: None,
        gateway: None,
        ..ipv4_route_request(destination, None)
    };
    del_route_state(selector).unwrap();

    let remaining = SERVICE
        .lock()
        .route_snapshot()
        .into_iter()
        .filter(|route| {
            route.is_configured() && route.filter == IpCidr::Ipv4(Ipv4Cidr::new(destination, 24))
        })
        .count();
    assert_eq!(remaining, 1);
}

#[def_test(serial)]
fn test_create_excl_and_replace_flag_precedence() {
    let state = init_test_state();

    let address = AddrRequest {
        index: ifindex(2),
        family: wire_route::FAMILY_IPV4,
        prefix_len: 24,
        scope: wire_route::SCOPE_UNIVERSE,
        address: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 81)),
    };
    add_addr_state(address, NLM_F_EXCL).unwrap();
    assert_eq!(
        add_addr_state(
            AddrRequest {
                scope: wire_route::SCOPE_HOST,
                ..address
            },
            NLM_F_EXCL | NLM_F_REPLACE,
        ),
        Err(LinuxError::EEXIST)
    );
    assert!(SERVICE.lock().ipv4_addr_snapshots().iter().any(|snapshot| {
        snapshot.entry.addr.address() == Ipv4Address::new(192, 168, 1, 81)
            && snapshot.entry.scope == wire_route::SCOPE_UNIVERSE
    }));

    let destination = Ipv4Address::new(172, 23, 0, 0);
    let route = ipv4_route_request(destination, None);
    assert_eq!(
        add_route_state(route, NLM_F_REPLACE),
        Err(LinuxError::ENOENT)
    );
    add_route_state(route, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert_eq!(
        add_route_state(
            ipv4_route_request(destination, Some(Ipv4Address::new(192, 168, 1, 1))),
            NLM_F_EXCL | NLM_F_REPLACE,
        ),
        Err(LinuxError::EEXIST)
    );
    assert!(SERVICE.lock().route_snapshot().iter().any(|existing| {
        existing.is_configured()
            && existing.filter == IpCidr::Ipv4(Ipv4Cidr::new(destination, 24))
            && existing.via.is_none()
    }));

    let neighbor = NeighRequest {
        family: wire_route::FAMILY_IPV4,
        ifindex: ifindex(2),
        state: wire_neigh::STATE_PERMANENT,
        flags: 0,
        dst: IpAddress::Ipv4(Ipv4Address::new(192, 168, 1, 82)),
        lladdr: Some([1, 2, 3, 4, 5, 6]),
    };
    add_neigh_state(neighbor, NLM_F_CREATE | NLM_F_EXCL).unwrap();
    assert_eq!(
        add_neigh_state(
            NeighRequest {
                lladdr: Some([6, 5, 4, 3, 2, 1]),
                ..neighbor
            },
            NLM_F_EXCL | NLM_F_REPLACE,
        ),
        Err(LinuxError::EEXIST)
    );
    assert!(state.eth0_neighbors.lock().iter().any(|existing| {
        existing.dst == neighbor.dst
            && existing.state
                == NeighborState::Permanent {
                    hardware_addr: neighbor.lladdr.unwrap(),
                }
    }));
}

#[def_test(serial)]
fn test_route_rejects_unreachable_gateway_without_state_change() {
    init_test_state();
    let destination = Ipv4Address::new(172, 24, 0, 0);
    let route = ipv4_route_request(destination, Some(Ipv4Address::new(198, 51, 100, 1)));

    assert_eq!(
        add_route_state(route, NLM_F_CREATE | NLM_F_EXCL),
        Err(LinuxError::ENETUNREACH)
    );
    assert!(!SERVICE.lock().route_snapshot().iter().any(|existing| {
        existing.is_configured() && existing.filter == IpCidr::Ipv4(Ipv4Cidr::new(destination, 24))
    }));

    let direct_destination = Ipv4Address::new(172, 25, 0, 0);
    assert_eq!(
        SERVICE.lock().add_route_rule(Rule::with_route_attrs(
            Ipv4Cidr::new(direct_destination, 24).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 1))),
            1,
            route_attrs(ROUTE_SCOPE_UNIVERSE, None),
        )),
        Err(LinuxError::ENETUNREACH)
    );
    assert!(!SERVICE.lock().route_snapshot().iter().any(|existing| {
        existing.is_configured()
            && existing.filter == IpCidr::Ipv4(Ipv4Cidr::new(direct_destination, 24))
    }));
}

#[def_test(serial)]
fn test_route_rejects_known_unsupported_priority_attribute() {
    init_test_state();
    let destination = Ipv4Address::new(172, 25, 0, 0);
    let mut payload = Vec::new();
    RtMsg {
        family: wire_route::FAMILY_IPV4,
        dst_len: 24,
        src_len: 0,
        tos: 0,
        table: wire_route::TABLE_MAIN,
        protocol: wire_route::PROTOCOL_BOOT,
        scope: wire_route::SCOPE_UNIVERSE,
        route_type: wire_route::TYPE_UNICAST,
        flags: 0,
    }
    .write(&mut payload);
    push_attr(&mut payload, wire_route::attr::DST, &destination.octets());
    push_attr(&mut payload, wire_route::attr::OIF, &2u32.to_ne_bytes());
    push_attr(
        &mut payload,
        wire_route::attr::PRIORITY,
        &100u32.to_ne_bytes(),
    );
    let request = build_nlmsg(
        RTM_NEWROUTE,
        56,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        payload,
    );

    let packets = handle_rtnetlink_request(&request);
    assert_eq!(packets.len(), 1);
    assert_eq!(read_u16_ne(&packets[0].data, 4), Some(NLMSG_ERROR));
    assert_eq!(
        read_i32_ne(&packets[0].data, NLMSG_HDR_LEN),
        Some(-(LinuxError::EOPNOTSUPP.into_raw()))
    );
    assert!(!SERVICE.lock().route_snapshot().iter().any(|existing| {
        existing.is_configured() && existing.filter == IpCidr::Ipv4(Ipv4Cidr::new(destination, 24))
    }));
}
