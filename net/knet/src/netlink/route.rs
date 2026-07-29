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
    device::{
        ETHERNET_MAX_MTU, IPV4_MIN_MTU, LINK_FLAG_UP, LOOPBACK_MAX_MTU, LinkKind, LinkSnapshot,
        NeighborState, NeighborUpdate,
    },
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
    router::{
        Ipv4AddrEntry, ROUTE_TABLE_MAIN, ROUTE_TYPE_UNICAST, RouteAttrs as RouterRouteAttrs, Rule,
    },
    service::Service,
};

const IFNAME_MAX_LEN: usize = 15;
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
    pub(super) src_len: u8,
    pub(super) table: u32,
    pub(super) protocol: u8,
    pub(super) scope: u8,
    pub(super) route_type: u8,
    pub(super) oif: u32,
    pub(super) dst: Option<IpAddress>,
    pub(super) src: Option<IpAddress>,
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
    src: Option<IpAddress>,
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
    ensure_service_inited()?;
    Ok(SERVICE
        .lock()
        .link_snapshots()
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
    if family != wire_route::FAMILY_IPV4 {
        return Ok(Vec::new());
    }
    ensure_service_inited()?;
    Ok(SERVICE
        .lock()
        .ipv4_addr_snapshots()
        .into_iter()
        .map(|snapshot| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_addr_message(request, snapshot.entry, &snapshot.label),
        })
        .collect())
}

fn dump_routes(request: &NlMsgHeader, payload: &[u8]) -> Result<Vec<NetlinkPacket>, LinuxError> {
    let family = payload
        .first()
        .copied()
        .filter(|family| *family != 0)
        .unwrap_or(wire_route::FAMILY_IPV4);
    ensure_service_inited()?;
    Ok(SERVICE
        .lock()
        .route_snapshot()
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
    if request.change & !LINK_FLAG_UP != 0 {
        return Err(LinuxError::EOPNOTSUPP);
    }
    let mut service = SERVICE.lock();
    let link = service
        .link_snapshot_for_ifindex(request.index)
        .ok_or(LinuxError::ENODEV)?;
    if let Some(name) = request.name.as_deref() {
        validate_interface_name(name)?;
    }
    if let Some(mtu) = request.mtu {
        validate_link_mtu(link.kind, mtu)?;
    }

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
    if existing {
        if flags & NLM_F_EXCL != 0 || flags & NLM_F_REPLACE == 0 {
            return Err(LinuxError::EEXIST);
        }
        return Ok(());
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
        .find(|route| same_route_key(*route, &request));
    if let Some(existing) = existing {
        if flags & NLM_F_EXCL != 0 || flags & NLM_F_REPLACE == 0 || !existing.is_configured() {
            return Err(LinuxError::EEXIST);
        }
        service.remove_route_rule(existing)?;
    } else if flags & NLM_F_CREATE == 0 {
        return Err(LinuxError::ENOENT);
    }
    service.add_route_rule(rule)
}

pub(super) fn del_route_state(request: RouteRequest) -> Result<(), LinuxError> {
    validate_route_request(request)?;
    if normalize_route_table(request.table) != u32::from(ROUTE_TABLE_MAIN)
        || normalize_route_type(request.route_type) != ROUTE_TYPE_UNICAST
    {
        return Err(LinuxError::EOPNOTSUPP);
    }
    ensure_service_inited()?;
    let mut service = SERVICE.lock();
    if request.oif != 0 {
        let dev = request.oif.checked_sub(1).ok_or(LinuxError::ENODEV)? as usize;
        if !service.has_device(dev) {
            return Err(LinuxError::ENODEV);
        }
    }
    let removed_route = service
        .route_rules()
        .iter()
        .copied()
        .find(|route| route.is_configured() && route_delete_matches(*route, &request))
        .ok_or(LinuxError::ESRCH)?;
    service.remove_route_rule(removed_route)?;
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
    if existing && (flags & NLM_F_EXCL != 0 || flags & NLM_F_REPLACE == 0) {
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

fn same_route_key(route: Rule, request: &RouteRequest) -> bool {
    same_route_destination(route, request)
        && route.route_type == normalize_route_type(request.route_type)
}

fn same_route_destination(route: Rule, request: &RouteRequest) -> bool {
    route_family(route) == request.family
        && route.filter.prefix_len() == request.dst_len
        && u32::from(route.table) == normalize_route_table(request.table)
        && route_dst(route) == normalized_route_request_dst(request)
}

fn route_delete_matches(route: Rule, request: &RouteRequest) -> bool {
    same_route_destination(route, request)
        && (request.route_type == 0 || route.route_type == request.route_type)
        && (request.protocol == 0 || route.protocol == request.protocol)
        && (request.scope == wire_route::SCOPE_NOWHERE || route.scope == request.scope)
        && (request.oif == 0 || route.dev as u32 + 1 == request.oif)
        && request
            .gateway
            .is_none_or(|gateway| route.via == Some(gateway))
        && request
            .prefsrc
            .is_none_or(|prefsrc| route.preferred_source() == Some(prefsrc))
}

fn normalized_route_request_dst(request: &RouteRequest) -> Option<IpAddress> {
    let dst = match request
        .dst
        .unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED))
    {
        IpAddress::Ipv4(addr) => {
            IpAddress::Ipv4(Ipv4Cidr::new(addr, request.dst_len).network().address())
        }
        IpAddress::Ipv6(addr) => IpAddress::Ipv6(addr),
    };
    (!dst.is_unspecified() || request.dst_len != 0).then_some(dst)
}

fn neighbor_update_from_request(request: NeighRequest) -> Result<NeighborUpdate, LinuxError> {
    if request.flags != 0 {
        return Err(LinuxError::EOPNOTSUPP);
    }
    let state = match normalize_neigh_state(request.state) {
        wire_neigh::STATE_INCOMPLETE => NeighborState::Incomplete,
        wire_neigh::STATE_PERMANENT => NeighborState::Permanent {
            hardware_addr: request.lladdr.ok_or(LinuxError::EINVAL)?,
        },
        _ => return Err(LinuxError::EOPNOTSUPP),
    };
    Ok(NeighborUpdate {
        dev: request.ifindex.checked_sub(1).ok_or(LinuxError::ENODEV)? as usize,
        dst: request.dst,
        state,
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
    if info.flags != 0 {
        return Err(LinuxError::EOPNOTSUPP);
    }
    let attrs = parse_addr_attrs(info.family, parse_attrs(&payload[IfAddrMsg::SIZE..])?)?;
    if let (Some(address), Some(local)) = (attrs.address, attrs.local)
        && address != local
    {
        return Err(LinuxError::EOPNOTSUPP);
    }
    Ok(AddrRequest {
        index: info.index,
        family: info.family,
        prefix_len: info.prefix_len,
        scope: info.scope,
        address: attrs.local.or(attrs.address).ok_or(LinuxError::EINVAL)?,
    })
}

pub(super) fn parse_route_request(payload: &[u8]) -> Result<RouteRequest, LinuxError> {
    let info = RtMsg::read(payload)?;
    if info.tos != 0 || info.flags != 0 {
        return Err(LinuxError::EOPNOTSUPP);
    }
    let attrs = parse_route_attrs(info.family, parse_attrs(&payload[RtMsg::SIZE..])?)?;
    Ok(RouteRequest {
        family: info.family,
        dst_len: info.dst_len,
        src_len: info.src_len,
        table: attrs.table.unwrap_or(u32::from(info.table)),
        protocol: info.protocol,
        scope: info.scope,
        route_type: info.route_type,
        oif: attrs.oif,
        dst: attrs.dst,
        src: attrs.src,
        gateway: attrs.gateway,
        prefsrc: attrs.prefsrc,
    })
}

fn parse_neigh_request(payload: &[u8]) -> Result<NeighRequest, LinuxError> {
    let info = NdMsg::read(payload)?;
    if info.neigh_type != 0 {
        return Err(LinuxError::EOPNOTSUPP);
    }
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
            | wire_link::attr::OPERSTATE => return Err(LinuxError::EOPNOTSUPP),
            _ => return Err(LinuxError::EOPNOTSUPP),
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
                return Err(LinuxError::EOPNOTSUPP);
            }
            _ => return Err(LinuxError::EOPNOTSUPP),
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
            wire_route::attr::SRC => parsed.src = Some(parse_ip_by_family(family, attr.payload)?),
            wire_route::attr::PREFSRC => {
                parsed.prefsrc = Some(parse_ip_by_family(family, attr.payload)?)
            }
            wire_route::attr::TABLE => parsed.table = Some(read_u32_payload(attr.payload)?),
            wire_route::attr::IIF
            | wire_route::attr::PRIORITY
            | wire_route::attr::METRICS
            | wire_route::attr::MULTIPATH
            | wire_route::attr::PROTOINFO
            | wire_route::attr::FLOW
            | wire_route::attr::CACHEINFO
            | wire_route::attr::SESSION
            | wire_route::attr::MP_ALGO
            | wire_route::attr::MARK
            | wire_route::attr::MFC_STATS
            | wire_route::attr::VIA
            | wire_route::attr::NEWDST
            | wire_route::attr::PREF
            | wire_route::attr::ENCAP_TYPE
            | wire_route::attr::ENCAP
            | wire_route::attr::EXPIRES
            | wire_route::attr::UID
            | wire_route::attr::TTL_PROPAGATE
            | wire_route::attr::IP_PROTO
            | wire_route::attr::SPORT
            | wire_route::attr::DPORT
            | wire_route::attr::NH_ID => return Err(LinuxError::EOPNOTSUPP),
            wire_route::attr::PAD => {}
            _ => return Err(LinuxError::EOPNOTSUPP),
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
            _ => return Err(LinuxError::EOPNOTSUPP),
        }
    }
    Ok(parsed)
}

fn build_link_message(request: &NlMsgHeader, link: LinkSnapshot) -> Vec<u8> {
    let mut payload = Vec::new();
    IfInfoMsg {
        family: 0,
        pad: 0,
        link_type: link_type(&link),
        index: link.ifindex,
        flags: link.flags,
        change: 0,
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
    if let Some(prefsrc) = route.preferred_source() {
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
    if request.prefix_len > 32 {
        return Err(LinuxError::EINVAL);
    }
    match request.address {
        IpAddress::Ipv4(ipv4) => Ok(Ipv4Cidr::new(ipv4, request.prefix_len)),
        IpAddress::Ipv6(_) => Err(LinuxError::EAFNOSUPPORT),
    }
}

fn route_rule_from_request(request: RouteRequest, service: &Service) -> Result<Rule, LinuxError> {
    validate_route_request(request)?;
    if normalize_route_table(request.table) != u32::from(ROUTE_TABLE_MAIN)
        || normalize_route_type(request.route_type) != ROUTE_TYPE_UNICAST
    {
        return Err(LinuxError::EOPNOTSUPP);
    }
    let filter = match request
        .dst
        .unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED))
    {
        IpAddress::Ipv4(addr) => IpCidr::Ipv4(Ipv4Cidr::new(addr, request.dst_len).network()),
        IpAddress::Ipv6(_) => return Err(LinuxError::EAFNOSUPPORT),
    };
    let dev = request.oif.checked_sub(1).ok_or(LinuxError::EOPNOTSUPP)? as usize;
    if !service.has_device(dev) {
        return Err(LinuxError::ENODEV);
    }
    if let Some(prefsrc) = request.prefsrc
        && !service.has_ipv4_address(prefsrc)
    {
        return Err(LinuxError::EINVAL);
    }
    if let Some(gateway) = request.gateway {
        let IpAddress::Ipv4(gateway_addr) = gateway else {
            return Err(LinuxError::EAFNOSUPPORT);
        };
        if !gateway_addr.is_unicast()
            || service.is_ipv4_directed_broadcast_for_device(dev, gateway_addr)
        {
            return Err(LinuxError::EINVAL);
        }
        if !service.has_ipv4_subnet_for_device(dev, gateway) {
            return Err(LinuxError::ENETUNREACH);
        }
    }

    Ok(Rule::with_route_attrs(
        filter,
        request.gateway,
        dev,
        RouterRouteAttrs {
            table: ROUTE_TABLE_MAIN,
            protocol: normalize_route_protocol(request.protocol),
            scope: request.scope,
            route_type: normalize_route_type(request.route_type),
            prefsrc: request.prefsrc,
        },
    ))
}

fn validate_route_request(request: RouteRequest) -> Result<(), LinuxError> {
    if request.family != wire_route::FAMILY_IPV4 {
        return Err(LinuxError::EAFNOSUPPORT);
    }
    if request.dst_len > 32 || request.src_len > 32 {
        return Err(LinuxError::EINVAL);
    }
    match (request.src_len, request.src) {
        (0, None) => Ok(()),
        (0, Some(_)) => Err(LinuxError::EINVAL),
        (..) => Err(LinuxError::EOPNOTSUPP),
    }
}

fn validate_interface_name(name: &str) -> Result<(), LinuxError> {
    let is_invalid = name.is_empty()
        || name.len() > IFNAME_MAX_LEN
        || matches!(name, "." | "..")
        || name
            .bytes()
            .any(|byte| byte == b'/' || byte == b':' || byte.is_ascii_whitespace());
    (!is_invalid).then_some(()).ok_or(LinuxError::EINVAL)
}

fn validate_link_mtu(kind: LinkKind, mtu: u32) -> Result<(), LinuxError> {
    let max_mtu = match kind {
        LinkKind::Loopback => LOOPBACK_MAX_MTU as u32,
        LinkKind::Ethernet => ETHERNET_MAX_MTU as u32,
    };
    (IPV4_MIN_MTU as u32..=max_mtu)
        .contains(&mtu)
        .then_some(())
        .ok_or(LinuxError::EINVAL)
}

fn ensure_service_inited() -> Result<(), LinuxError> {
    SERVICE.is_inited().then_some(()).ok_or(LinuxError::ENODEV)
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
