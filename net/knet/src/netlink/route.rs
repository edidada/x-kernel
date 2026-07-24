// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, vec, vec::Vec};

use kerrno::LinuxError;

use super::{
    wire::{
        IfAddrMsg, IfInfoMsg, NdMsg, NlAttr, NlMsgHeader, RtMsg, addr as wire_addr, build_nlmsg,
        build_nlmsg_done_response, build_nlmsg_error_response, ip_addr_bytes, link as wire_link,
        neigh as wire_neigh, normalize_neigh_state, normalize_route_protocol,
        normalize_route_table, normalize_route_type, parse_attrs, parse_ip_by_family, parse_mac,
        parse_string, push_attr, push_attr_str, read_u32_payload, route as wire_route,
    },
    *,
};
use crate::{
    device::{LINK_FLAG_UP, LinkKind, LinkSnapshot, NeighborUpdate},
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
    router::{
        Ipv4AddrEntry, NetworkSnapshot, ROUTE_TABLE_MAIN, ROUTE_TYPE_UNICAST,
        RouteAttrs as RouterRouteAttrs, Rule,
    },
    service::Service,
};

#[derive(Clone, Debug)]
pub(super) struct LinkUpdateRequest {
    pub(super) index: i32,
    pub(super) flags: u32,
    pub(super) change: u32,
    pub(super) name: Option<String>,
    pub(super) mtu: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct AddrRequest {
    pub(super) index: u32,
    pub(super) family: u8,
    pub(super) prefix_len: u8,
    pub(super) scope: u8,
    pub(super) address: IpAddress,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RouteRequest {
    pub(super) family: u8,
    pub(super) dst_len: u8,
    pub(super) table: u32,
    pub(super) protocol: u8,
    pub(super) scope: u8,
    pub(super) route_type: u8,
    pub(super) oif: u32,
    pub(super) dst: Option<IpAddress>,
    pub(super) gateway: Option<IpAddress>,
    pub(super) prefsrc: Option<IpAddress>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct NeighRequest {
    pub(super) family: u8,
    pub(super) ifindex: u32,
    pub(super) state: u16,
    pub(super) flags: u8,
    pub(super) dst: IpAddress,
    pub(super) lladdr: Option<[u8; 6]>,
}

#[derive(Clone, Debug, Default)]
struct LinkAttrs {
    name: Option<String>,
    mtu: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
struct AddrAttrs {
    address: Option<IpAddress>,
    local: Option<IpAddress>,
}

#[derive(Clone, Copy, Debug, Default)]
struct RouteRequestAttrs {
    oif: u32,
    dst: Option<IpAddress>,
    gateway: Option<IpAddress>,
    prefsrc: Option<IpAddress>,
    table: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
struct NeighAttrs {
    dst: Option<IpAddress>,
    lladdr: Option<[u8; 6]>,
}

pub(super) fn build_error_response(request: &[u8], errno: LinuxError) -> Vec<u8> {
    let Some(header) = NlMsgHeader::read(request) else {
        return Vec::new();
    };
    build_nlmsg_error_response(&header, -errno.into_raw(), request)
}

pub(super) fn build_ack_response(request: &[u8]) -> Vec<u8> {
    let Some(header) = NlMsgHeader::read(request) else {
        return Vec::new();
    };
    build_nlmsg_error_response(&header, 0, request)
}

pub(super) fn handle_route_request(request: &[u8]) -> Vec<NetlinkPacket> {
    let Some(header) = NlMsgHeader::read(request) else {
        return Vec::new();
    };
    if header.flags & NLM_F_REQUEST == 0 {
        return vec![NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_error_response(request, LinuxError::EINVAL),
        }];
    }

    let payload = request
        .get(NLMSG_HDR_LEN..(header.len as usize).min(request.len()))
        .unwrap_or(&[]);
    match header.msg_type {
        RTM_GETLINK => finish_dump(request, &header, dump_links(&header, payload)),
        RTM_GETADDR => finish_dump(request, &header, dump_addrs(&header, payload)),
        RTM_GETROUTE => finish_dump(request, &header, dump_routes(&header, payload)),
        RTM_NEWLINK => apply_newlink(request, &header, payload),
        RTM_NEWADDR => apply_newaddr(request, &header, payload),
        RTM_DELADDR => apply_deladdr(request, &header, payload),
        RTM_NEWROUTE => apply_newroute(request, &header, payload),
        RTM_DELROUTE => apply_delroute(request, &header, payload),
        RTM_NEWNEIGH => apply_newneigh(request, &header, payload),
        _ => vec![NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_error_response(request, LinuxError::EOPNOTSUPP),
        }],
    }
}

pub(super) fn finish_dump(
    request: &[u8],
    header: &NlMsgHeader,
    result: Result<Vec<NetlinkPacket>, LinuxError>,
) -> Vec<NetlinkPacket> {
    match result {
        Ok(mut packets) => {
            packets.push(NetlinkPacket {
                from: NetlinkAddr { pid: 0, groups: 0 },
                data: build_done_response(header),
            });
            packets
        }
        Err(errno) => error_packets(request, errno),
    }
}

fn dump_links(request: &NlMsgHeader, _payload: &[u8]) -> Result<Vec<NetlinkPacket>, LinuxError> {
    Ok(network_snapshot()?
        .links
        .into_iter()
        .map(|link| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_link_message(request, link),
        })
        .collect())
}

fn dump_addrs(request: &NlMsgHeader, payload: &[u8]) -> Result<Vec<NetlinkPacket>, LinuxError> {
    let family = payload
        .first()
        .copied()
        .filter(|family| *family != 0)
        .unwrap_or(wire_route::FAMILY_IPV4);
    let NetworkSnapshot { links, addrs, .. } = network_snapshot()?;
    if family != wire_route::FAMILY_IPV4 {
        return Ok(Vec::new());
    }
    Ok(addrs
        .into_iter()
        .map(|addr| {
            let label = links
                .iter()
                .find(|link| link.ifindex as usize == addr.dev + 1)
                .map(|link| link.name.as_str())
                .unwrap_or("");
            NetlinkPacket {
                from: NetlinkAddr { pid: 0, groups: 0 },
                data: build_addr_message(request, addr, label),
            }
        })
        .collect())
}

fn dump_routes(request: &NlMsgHeader, payload: &[u8]) -> Result<Vec<NetlinkPacket>, LinuxError> {
    let family = payload
        .first()
        .copied()
        .filter(|family| *family != 0)
        .unwrap_or(wire_route::FAMILY_IPV4);
    Ok(network_snapshot()?
        .routes
        .into_iter()
        .filter(|route| route_family(*route) == family)
        .map(|route| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_route_message(request, route),
        })
        .collect())
}

fn apply_newlink(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_link_update(payload).and_then(update_link_state) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_newaddr(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_addr_request(payload).and_then(|req| add_addr_state(req, header.flags)) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_deladdr(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_addr_request(payload).and_then(del_addr_state) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_newroute(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_route_request(payload).and_then(|req| add_route_state(req, header.flags)) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_delroute(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_route_request(payload).and_then(del_route_state) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

fn apply_newneigh(request: &[u8], header: &NlMsgHeader, payload: &[u8]) -> Vec<NetlinkPacket> {
    match parse_neigh_request(payload).and_then(|req| add_neigh_state(req, header.flags)) {
        Ok(()) => ack_packets(request, header),
        Err(errno) => error_packets(request, errno),
    }
}

pub(super) fn update_link_state(request: LinkUpdateRequest) -> Result<(), LinuxError> {
    ensure_service_inited()?;
    let dev = dev_index_from_link_index(request.index).ok_or(LinuxError::ENODEV)?;
    let mut service = SERVICE.lock();
    if let Some(name) = request.name {
        service.set_device_name(dev, name)?;
    }
    if let Some(mtu) = request.mtu {
        service.set_device_mtu(dev, mtu as usize)?;
    }
    if request.change & LINK_FLAG_UP != 0 {
        service.set_device_link_up(dev, request.flags & LINK_FLAG_UP != 0)?;
    }
    Ok(())
}

pub(super) fn add_addr_state(request: AddrRequest, flags: u16) -> Result<(), LinuxError> {
    let dev = dev_index_from_ifindex(request.index).ok_or(LinuxError::ENODEV)?;
    let addr = ipv4_cidr_from_addr_request(request)?;
    ensure_service_inited()?;
    let mut service = SERVICE.lock();
    ensure_link_exists(&service, request.index)?;
    let existing = service
        .ipv4_addr_entries()
        .iter()
        .any(|entry| same_addr_entry(*entry, &request));
    if existing && flags & NLM_F_REPLACE == 0 {
        return Err(LinuxError::EEXIST);
    }
    if !existing && flags & NLM_F_CREATE == 0 && flags & NLM_F_EXCL != 0 {
        return Err(LinuxError::ENOENT);
    }
    service.add_ipv4_addr(Ipv4AddrEntry {
        dev,
        addr,
        scope: request.scope,
    })
}

pub(super) fn del_addr_state(request: AddrRequest) -> Result<(), LinuxError> {
    let dev = dev_index_from_ifindex(request.index).ok_or(LinuxError::ENODEV)?;
    let addr = ipv4_cidr_from_addr_request(request)?;
    ensure_service_inited()?;
    let mut service = SERVICE.lock();
    let Some(entry) = service
        .ipv4_addr_entries()
        .iter()
        .copied()
        .find(|entry| same_addr_entry(*entry, &request))
    else {
        return Err(LinuxError::EADDRNOTAVAIL);
    };
    service.remove_ipv4_addr(Ipv4AddrEntry { dev, addr, ..entry })?;
    Ok(())
}

pub(super) fn add_route_state(request: RouteRequest, flags: u16) -> Result<(), LinuxError> {
    ensure_service_inited()?;
    let mut service = SERVICE.lock();
    let rule = route_rule_from_request(request, &service)?;
    let existing = service
        .route_rules()
        .iter()
        .copied()
        .find(|route| same_route_rule(*route, &request));
    if let Some(existing) = existing {
        if flags & NLM_F_REPLACE == 0 {
            return Err(LinuxError::EEXIST);
        }
        service.remove_route_rule(existing)?;
    }
    service.add_route_rule(rule)
}

pub(super) fn del_route_state(request: RouteRequest) -> Result<(), LinuxError> {
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    ensure_service_inited()?;
    let mut service = SERVICE.lock();
    let removed_routes: Vec<_> = service
        .route_rules()
        .iter()
        .copied()
        .filter(|route| same_route_rule(*route, &request))
        .collect();
    if removed_routes.is_empty() {
        return Err(LinuxError::ESRCH);
    }
    for route in removed_routes {
        service.remove_route_rule(route)?;
    }
    Ok(())
}

pub(super) fn add_neigh_state(request: NeighRequest, flags: u16) -> Result<(), LinuxError> {
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    ensure_service_inited()?;
    let mut service = SERVICE.lock();
    ensure_link_exists(&service, request.ifindex)?;
    let update = neighbor_update_from_request(request)?;
    let existing = service.has_neighbor(update.dev, update.dst);
    if existing && flags & NLM_F_REPLACE == 0 {
        return Err(LinuxError::EEXIST);
    }
    if !existing && flags & NLM_F_CREATE == 0 {
        return Err(LinuxError::ENOENT);
    }
    service.apply_neighbor_update(update)
}

fn ensure_link_exists(service: &Service, ifindex: u32) -> Result<(), LinuxError> {
    dev_index_from_ifindex(ifindex)
        .is_some_and(|dev| service.has_device(dev))
        .then_some(())
        .ok_or(LinuxError::ENODEV)
}

fn same_addr_entry(entry: Ipv4AddrEntry, request: &AddrRequest) -> bool {
    entry.dev as u32 + 1 == request.index
        && request.family == wire_route::FAMILY_IPV4
        && entry.addr.prefix_len() == request.prefix_len
        && IpAddress::Ipv4(entry.addr.address()) == request.address
}

fn same_route_rule(route: Rule, request: &RouteRequest) -> bool {
    route_family(route) == request.family
        && route.filter.prefix_len() == request.dst_len
        && u32::from(route.table) == normalize_route_table(request.table)
        && route.protocol == normalize_route_protocol(request.protocol)
        && route.scope == request.scope
        && route.route_type == normalize_route_type(request.route_type)
        && route.dev as u32 + 1 == request.oif
        && route_dst(route) == request.dst
        && route.via == request.gateway
        && route.prefsrc == request.prefsrc
}

fn neighbor_update_from_request(request: NeighRequest) -> Result<NeighborUpdate, LinuxError> {
    if request.flags != 0 {
        return Err(LinuxError::EOPNOTSUPP);
    }
    let hardware_addr = match normalize_neigh_state(request.state) {
        wire_neigh::STATE_INCOMPLETE => None,
        wire_neigh::STATE_PERMANENT => Some(request.lladdr.ok_or(LinuxError::EINVAL)?),
        _ => return Err(LinuxError::EOPNOTSUPP),
    };
    Ok(NeighborUpdate {
        dev: request.ifindex.checked_sub(1).ok_or(LinuxError::ENODEV)? as usize,
        dst: request.dst,
        hardware_addr,
    })
}

pub(super) fn parse_link_update(payload: &[u8]) -> Result<LinkUpdateRequest, LinuxError> {
    let info = IfInfoMsg::read(payload)?;
    let attrs = parse_link_attrs(parse_attrs(&payload[IfInfoMsg::SIZE..])?)?;
    Ok(LinkUpdateRequest {
        index: info.index,
        flags: info.flags,
        change: info.change,
        name: attrs.name,
        mtu: attrs.mtu,
    })
}

fn parse_addr_request(payload: &[u8]) -> Result<AddrRequest, LinuxError> {
    let info = IfAddrMsg::read(payload)?;
    let attrs = parse_addr_attrs(info.family, parse_attrs(&payload[IfAddrMsg::SIZE..])?)?;
    Ok(AddrRequest {
        index: info.index,
        family: info.family,
        prefix_len: info.prefix_len,
        scope: info.scope,
        address: attrs.local.or(attrs.address).ok_or(LinuxError::EINVAL)?,
    })
}

fn parse_route_request(payload: &[u8]) -> Result<RouteRequest, LinuxError> {
    let info = RtMsg::read(payload)?;
    let attrs = parse_route_attrs(info.family, parse_attrs(&payload[RtMsg::SIZE..])?)?;
    Ok(RouteRequest {
        family: info.family,
        dst_len: info.dst_len,
        table: attrs.table.unwrap_or(u32::from(info.table)),
        protocol: info.protocol,
        scope: info.scope,
        route_type: info.route_type,
        oif: attrs.oif,
        dst: attrs.dst,
        gateway: attrs.gateway,
        prefsrc: attrs.prefsrc,
    })
}

fn parse_neigh_request(payload: &[u8]) -> Result<NeighRequest, LinuxError> {
    let info = NdMsg::read(payload)?;
    let attrs = parse_neigh_attrs(info.family, parse_attrs(&payload[NdMsg::SIZE..])?)?;
    Ok(NeighRequest {
        family: info.family,
        ifindex: info.ifindex as u32,
        state: info.state,
        flags: info.flags,
        dst: attrs.dst.ok_or(LinuxError::EINVAL)?,
        lladdr: attrs.lladdr,
    })
}

fn parse_link_attrs(attrs: Vec<NlAttr<'_>>) -> Result<LinkAttrs, LinuxError> {
    let mut parsed = LinkAttrs::default();
    for attr in attrs {
        match attr.kind {
            wire_link::attr::IFNAME => parsed.name = Some(parse_string(attr.payload)?),
            wire_link::attr::MTU => parsed.mtu = Some(read_u32_payload(attr.payload)?),
            wire_link::attr::EXT_MASK
            | wire_link::attr::ADDRESS
            | wire_link::attr::BROADCAST
            | wire_link::attr::LINK
            | wire_link::attr::OPERSTATE => {}
            _ => {}
        }
    }
    Ok(parsed)
}

fn parse_addr_attrs(family: u8, attrs: Vec<NlAttr<'_>>) -> Result<AddrAttrs, LinuxError> {
    let mut parsed = AddrAttrs::default();
    for attr in attrs {
        match attr.kind {
            wire_addr::attr::LOCAL => {
                parsed.local = Some(parse_ip_by_family(family, attr.payload)?)
            }
            wire_addr::attr::ADDRESS => {
                parsed.address = Some(parse_ip_by_family(family, attr.payload)?)
            }
            wire_addr::attr::LABEL => {
                let _ = parse_string(attr.payload)?;
            }
            _ => {}
        }
    }
    Ok(parsed)
}

fn parse_route_attrs(family: u8, attrs: Vec<NlAttr<'_>>) -> Result<RouteRequestAttrs, LinuxError> {
    let mut parsed = RouteRequestAttrs::default();
    for attr in attrs {
        match attr.kind {
            wire_route::attr::DST => parsed.dst = Some(parse_ip_by_family(family, attr.payload)?),
            wire_route::attr::OIF => parsed.oif = read_u32_payload(attr.payload)?,
            wire_route::attr::GATEWAY => {
                parsed.gateway = Some(parse_ip_by_family(family, attr.payload)?)
            }
            wire_route::attr::PREFSRC | wire_route::attr::SRC => {
                parsed.prefsrc = Some(parse_ip_by_family(family, attr.payload)?)
            }
            wire_route::attr::TABLE => parsed.table = Some(read_u32_payload(attr.payload)?),
            _ => {}
        }
    }
    Ok(parsed)
}

fn parse_neigh_attrs(family: u8, attrs: Vec<NlAttr<'_>>) -> Result<NeighAttrs, LinuxError> {
    let mut parsed = NeighAttrs::default();
    for attr in attrs {
        match attr.kind {
            wire_neigh::attr::DST => parsed.dst = Some(parse_ip_by_family(family, attr.payload)?),
            wire_neigh::attr::LLADDR => parsed.lladdr = Some(parse_mac(attr.payload)?),
            _ => {}
        }
    }
    Ok(parsed)
}

fn build_link_message(request: &NlMsgHeader, link: LinkSnapshot) -> Vec<u8> {
    let mut payload = Vec::new();
    IfInfoMsg {
        family: 17,
        pad: 0,
        link_type: link_type(&link),
        index: link.ifindex,
        flags: link.flags,
        change: u32::MAX,
    }
    .write(&mut payload);

    push_attr(&mut payload, wire_link::attr::ADDRESS, &link.hardware_addr);
    push_attr(
        &mut payload,
        wire_link::attr::BROADCAST,
        &link.broadcast_addr,
    );
    push_attr_str(&mut payload, wire_link::attr::IFNAME, &link.name);
    push_attr(
        &mut payload,
        wire_link::attr::MTU,
        &(link.mtu as u32).to_ne_bytes(),
    );
    if link.ifindex > 1 {
        push_attr(
            &mut payload,
            wire_link::attr::LINK,
            &(link.ifindex as u32 - 1).to_ne_bytes(),
        );
    }
    push_attr(&mut payload, wire_link::attr::OPERSTATE, &[link.operstate]);

    build_nlmsg(RTM_NEWLINK, request.seq, NLM_F_MULTI, payload)
}

fn build_addr_message(request: &NlMsgHeader, addr: Ipv4AddrEntry, label: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    IfAddrMsg {
        family: wire_route::FAMILY_IPV4,
        prefix_len: addr.addr.prefix_len(),
        flags: 0,
        scope: addr.scope,
        index: addr.dev as u32 + 1,
    }
    .write(&mut payload);

    let ip_bytes = ip_addr_bytes(IpAddress::Ipv4(addr.addr.address()));
    push_attr(&mut payload, wire_addr::attr::ADDRESS, &ip_bytes);
    push_attr(&mut payload, wire_addr::attr::LOCAL, &ip_bytes);
    push_attr_str(&mut payload, wire_addr::attr::LABEL, label);

    build_nlmsg(RTM_NEWADDR, request.seq, NLM_F_MULTI, payload)
}

fn build_route_message(request: &NlMsgHeader, route: Rule) -> Vec<u8> {
    let mut payload = Vec::new();
    RtMsg {
        family: route_family(route),
        dst_len: route.filter.prefix_len(),
        src_len: 0,
        tos: 0,
        table: route.table,
        protocol: route.protocol,
        scope: route.scope,
        route_type: route.route_type,
        flags: 0,
    }
    .write(&mut payload);

    if let Some(dst) = route_dst(route) {
        let dst = ip_addr_bytes(dst);
        push_attr(&mut payload, wire_route::attr::DST, &dst);
    }
    push_attr(
        &mut payload,
        wire_route::attr::OIF,
        &(route.dev as u32 + 1).to_ne_bytes(),
    );
    if let Some(gateway) = route.via {
        let gateway = ip_addr_bytes(gateway);
        push_attr(&mut payload, wire_route::attr::GATEWAY, &gateway);
    }
    if let Some(prefsrc) = route.prefsrc {
        let prefsrc = ip_addr_bytes(prefsrc);
        push_attr(&mut payload, wire_route::attr::PREFSRC, &prefsrc);
    }

    build_nlmsg(RTM_NEWROUTE, request.seq, NLM_F_MULTI, payload)
}

fn build_done_response(request: &NlMsgHeader) -> Vec<u8> {
    build_nlmsg_done_response(request)
}

fn ack_packets(request: &[u8], header: &NlMsgHeader) -> Vec<NetlinkPacket> {
    if header.flags & NLM_F_ACK == 0 {
        Vec::new()
    } else {
        vec![NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_ack_response(request),
        }]
    }
}

fn error_packets(request: &[u8], errno: LinuxError) -> Vec<NetlinkPacket> {
    vec![NetlinkPacket {
        from: NetlinkAddr { pid: 0, groups: 0 },
        data: build_error_response(request, errno),
    }]
}

fn dev_index_from_link_index(ifindex: i32) -> Option<usize> {
    (ifindex > 0).then_some((ifindex - 1) as usize)
}

fn dev_index_from_ifindex(ifindex: u32) -> Option<usize> {
    ifindex.checked_sub(1).map(|index| index as usize)
}

fn ipv4_cidr_from_addr_request(request: AddrRequest) -> Result<Ipv4Cidr, LinuxError> {
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    match request.address {
        IpAddress::Ipv4(ipv4) => Ok(Ipv4Cidr::new(ipv4, request.prefix_len)),
        IpAddress::Ipv6(_) => Err(LinuxError::EAFNOSUPPORT),
    }
}

fn route_rule_from_request(request: RouteRequest, service: &Service) -> Result<Rule, LinuxError> {
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    if normalize_route_table(request.table) != u32::from(ROUTE_TABLE_MAIN)
        || normalize_route_type(request.route_type) != ROUTE_TYPE_UNICAST
    {
        return Err(LinuxError::EOPNOTSUPP);
    }
    let filter = match request
        .dst
        .unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED))
    {
        IpAddress::Ipv4(addr) => IpCidr::Ipv4(Ipv4Cidr::new(addr, request.dst_len)),
        IpAddress::Ipv6(_) => return Err(LinuxError::EAFNOSUPPORT),
    };
    let dev = request.oif.checked_sub(1).ok_or(LinuxError::EOPNOTSUPP)? as usize;
    let src = request
        .prefsrc
        .or_else(|| {
            service
                .first_ipv4_addr_for_device(dev)
                .map(|addr| IpAddress::Ipv4(addr.address()))
        })
        .unwrap_or(filter.address());
    let mtu = service
        .link_snapshot_for_ifindex(request.oif as i32)
        .map(|link| link.mtu)
        .ok_or(LinuxError::ENODEV)?;

    Ok(Rule::with_route_attrs(
        filter,
        request.gateway,
        dev,
        src,
        mtu,
        RouterRouteAttrs {
            table: ROUTE_TABLE_MAIN,
            protocol: normalize_route_protocol(request.protocol),
            scope: request.scope,
            route_type: normalize_route_type(request.route_type),
            prefsrc: request.prefsrc,
        },
    ))
}

fn ensure_service_inited() -> Result<(), LinuxError> {
    SERVICE.is_inited().then_some(()).ok_or(LinuxError::ENODEV)
}

fn network_snapshot() -> Result<NetworkSnapshot, LinuxError> {
    ensure_service_inited()?;
    Ok(SERVICE.lock().network_snapshot())
}

fn link_type(link: &LinkSnapshot) -> u16 {
    match link.kind {
        LinkKind::Loopback => ARPHRD_LOOPBACK,
        LinkKind::Ethernet => ARPHRD_ETHER,
    }
}

fn route_family(route: Rule) -> u8 {
    match route.filter.address() {
        IpAddress::Ipv4(_) => wire_route::FAMILY_IPV4,
        IpAddress::Ipv6(_) => wire_route::FAMILY_IPV6,
    }
}

fn route_dst(route: Rule) -> Option<IpAddress> {
    if route.filter.prefix_len() == 0 && route.filter.address().is_unspecified() {
        None
    } else {
        Some(route.filter.address())
    }
}
