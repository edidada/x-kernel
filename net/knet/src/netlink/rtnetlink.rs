// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Typed adapter for the supported `NETLINK_ROUTE` protocol subset.
//!
//! Raw ABI structures stay in [`super::wire`]. This module converts them into
//! [`RtnlRequest`] variants before mutation logic can access network state.

use alloc::{string::String, vec, vec::Vec};
use core::num::NonZeroU32;

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
        LINK_FLAG_UP, LinkConfigUpdate, LinkKind, LinkSnapshot, NeighborState, NeighborUpdate,
    },
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr},
    router::{
        Ipv4AddrEntry, ROUTE_TABLE_MAIN, ROUTE_TYPE_UNICAST, RouteAttrs as RouterRouteAttrs, Rule,
    },
    service::Service,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InterfaceIndex(NonZeroU32);

impl InterfaceIndex {
    pub(super) fn new(index: u32) -> Result<Self, LinuxError> {
        NonZeroU32::new(index).map(Self).ok_or(LinuxError::ENODEV)
    }

    fn from_signed(index: i32) -> Result<Self, LinuxError> {
        u32::try_from(index)
            .ok()
            .and_then(NonZeroU32::new)
            .map(Self)
            .ok_or(LinuxError::ENODEV)
    }

    fn get(self) -> u32 {
        self.0.get()
    }

    fn device_index(self) -> usize {
        (self.get() - 1) as usize
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct RequestFlags(u16);

impl RequestFlags {
    pub(super) const fn from_raw(flags: u16) -> Self {
        Self(flags)
    }

    fn requires_ack(self) -> bool {
        self.0 & NLM_F_ACK != 0
    }

    fn creates(self) -> bool {
        self.0 & NLM_F_CREATE != 0
    }

    fn replaces(self) -> bool {
        self.0 & NLM_F_REPLACE != 0
    }

    fn is_exclusive(self) -> bool {
        self.0 & NLM_F_EXCL != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AddressFamily {
    Ipv4,
    Other(u8),
}

impl AddressFamily {
    fn from_dump_payload(payload: &[u8]) -> Self {
        match payload.first().copied().filter(|family| *family != 0) {
            None | Some(wire_route::FAMILY_IPV4) => Self::Ipv4,
            Some(family) => Self::Other(family),
        }
    }

    fn matches(self, route: Rule) -> bool {
        match self {
            Self::Ipv4 => route_family(route) == wire_route::FAMILY_IPV4,
            Self::Other(family) => route_family(route) == family,
        }
    }
}

#[derive(Debug)]
enum RtnlRequest {
    DumpLinks,
    DumpAddresses(AddressFamily),
    DumpRoutes(AddressFamily),
    UpdateLink(LinkUpdateRequest),
    AddAddress(AddrRequest),
    DeleteAddress(AddrRequest),
    AddRoute(RouteRequest),
    DeleteRoute(RouteRequest),
    AddNeighbor(NeighRequest),
}

impl RtnlRequest {
    fn decode(header: &NlMsgHeader, payload: &[u8]) -> Result<Self, LinuxError> {
        match header.msg_type {
            RTM_GETLINK => Ok(Self::DumpLinks),
            RTM_GETADDR => Ok(Self::DumpAddresses(AddressFamily::from_dump_payload(
                payload,
            ))),
            RTM_GETROUTE => Ok(Self::DumpRoutes(AddressFamily::from_dump_payload(payload))),
            RTM_NEWLINK => LinkUpdateRequest::decode(payload).map(Self::UpdateLink),
            RTM_NEWADDR => AddrRequest::decode(payload).map(Self::AddAddress),
            RTM_DELADDR => AddrRequest::decode(payload).map(Self::DeleteAddress),
            RTM_NEWROUTE => RouteRequest::decode(payload).map(Self::AddRoute),
            RTM_DELROUTE => RouteRequest::decode(payload).map(Self::DeleteRoute),
            RTM_NEWNEIGH => NeighRequest::decode(payload).map(Self::AddNeighbor),
            _ => Err(LinuxError::EOPNOTSUPP),
        }
    }

    fn execute(
        self,
        request: &[u8],
        header: &NlMsgHeader,
        flags: RequestFlags,
    ) -> Vec<NetlinkPacket> {
        match self {
            Self::DumpLinks => finish_dump(request, header, dump_links(header)),
            Self::DumpAddresses(family) => finish_dump(request, header, dump_addrs(header, family)),
            Self::DumpRoutes(family) => finish_dump(request, header, dump_routes(header, family)),
            Self::UpdateLink(update) => finish_mutation(request, flags, update.apply()),
            Self::AddAddress(address) => finish_mutation(request, flags, address.add(flags)),
            Self::DeleteAddress(address) => finish_mutation(request, flags, address.delete()),
            Self::AddRoute(route) => finish_mutation(request, flags, route.add(flags)),
            Self::DeleteRoute(route) => finish_mutation(request, flags, route.delete()),
            Self::AddNeighbor(neighbor) => finish_mutation(request, flags, neighbor.add(flags)),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct LinkUpdateRequest {
    pub(super) index: InterfaceIndex,
    pub(super) flags: u32,
    pub(super) change: u32,
    pub(super) name: Option<String>,
    pub(super) mtu: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct AddrRequest {
    pub(super) index: InterfaceIndex,
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
    pub(super) oif: Option<InterfaceIndex>,
    pub(super) dst: Option<IpAddress>,
    pub(super) src: Option<IpAddress>,
    pub(super) gateway: Option<IpAddress>,
    pub(super) prefsrc: Option<IpAddress>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct NeighRequest {
    pub(super) family: u8,
    pub(super) ifindex: InterfaceIndex,
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

pub(super) fn handle_rtnetlink_request(request: &[u8]) -> Vec<NetlinkPacket> {
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
    let flags = RequestFlags::from_raw(header.flags);
    match RtnlRequest::decode(&header, payload) {
        Ok(request_kind) => request_kind.execute(request, &header, flags),
        Err(errno) => error_packets(request, errno),
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

fn dump_links(request: &NlMsgHeader) -> Result<Vec<NetlinkPacket>, LinuxError> {
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

fn dump_addrs(
    request: &NlMsgHeader,
    family: AddressFamily,
) -> Result<Vec<NetlinkPacket>, LinuxError> {
    if family != AddressFamily::Ipv4 {
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

fn dump_routes(
    request: &NlMsgHeader,
    family: AddressFamily,
) -> Result<Vec<NetlinkPacket>, LinuxError> {
    ensure_service_inited()?;
    Ok(SERVICE
        .lock()
        .route_snapshot()
        .into_iter()
        .filter(|route| family.matches(*route))
        .map(|route| NetlinkPacket {
            from: NetlinkAddr { pid: 0, groups: 0 },
            data: build_route_message(request, route),
        })
        .collect())
}

fn finish_mutation(
    request: &[u8],
    flags: RequestFlags,
    result: Result<(), LinuxError>,
) -> Vec<NetlinkPacket> {
    match result {
        Ok(()) => ack_packets(request, flags),
        Err(errno) => error_packets(request, errno),
    }
}

impl LinkUpdateRequest {
    pub(super) fn decode(payload: &[u8]) -> Result<Self, LinuxError> {
        let info = IfInfoMsg::read(payload)?;
        let attrs = LinkAttrs::decode(parse_attrs(&payload[IfInfoMsg::SIZE..])?)?;
        Ok(Self {
            index: InterfaceIndex::from_signed(info.index)?,
            flags: info.flags,
            change: info.change,
            name: attrs.name,
            mtu: attrs.mtu,
        })
    }

    pub(super) fn apply(self) -> Result<(), LinuxError> {
        ensure_service_inited()?;
        let dev = self.index.device_index();
        if self.change & !LINK_FLAG_UP != 0 {
            return Err(LinuxError::EOPNOTSUPP);
        }
        SERVICE.lock().update_device_link(
            dev,
            LinkConfigUpdate {
                name: self.name,
                mtu: self.mtu.map(|mtu| mtu as usize),
                is_up: (self.change & LINK_FLAG_UP != 0).then_some(self.flags & LINK_FLAG_UP != 0),
            },
        )
    }
}

impl AddrRequest {
    fn decode(payload: &[u8]) -> Result<Self, LinuxError> {
        let info = IfAddrMsg::read(payload)?;
        if info.flags != 0 {
            return Err(LinuxError::EOPNOTSUPP);
        }
        let attrs = AddrAttrs::decode(info.family, parse_attrs(&payload[IfAddrMsg::SIZE..])?)?;
        if let (Some(address), Some(local)) = (attrs.address, attrs.local)
            && address != local
        {
            return Err(LinuxError::EOPNOTSUPP);
        }
        Ok(Self {
            index: InterfaceIndex::new(info.index)?,
            family: info.family,
            prefix_len: info.prefix_len,
            scope: info.scope,
            address: attrs.local.or(attrs.address).ok_or(LinuxError::EINVAL)?,
        })
    }

    pub(super) fn add(self, flags: RequestFlags) -> Result<(), LinuxError> {
        let dev = self.index.device_index();
        let addr = ipv4_cidr_from_addr_request(self)?;
        ensure_service_inited()?;
        let mut service = SERVICE.lock();
        ensure_link_exists(&service, self.index)?;
        let existing = service
            .ipv4_addr_entries()
            .iter()
            .any(|entry| same_addr_entry(*entry, &self));
        if existing {
            if flags.is_exclusive() || !flags.replaces() {
                return Err(LinuxError::EEXIST);
            }
            return Ok(());
        }
        service.add_ipv4_addr(Ipv4AddrEntry {
            dev,
            addr,
            scope: self.scope,
        })
    }

    pub(super) fn delete(self) -> Result<(), LinuxError> {
        let dev = self.index.device_index();
        let addr = ipv4_cidr_from_addr_request(self)?;
        ensure_service_inited()?;
        let mut service = SERVICE.lock();
        let Some(entry) = service
            .ipv4_addr_entries()
            .iter()
            .copied()
            .find(|entry| same_addr_entry(*entry, &self))
        else {
            return Err(LinuxError::EADDRNOTAVAIL);
        };
        service.remove_ipv4_addr(Ipv4AddrEntry { dev, addr, ..entry })?;
        Ok(())
    }
}

impl RouteRequest {
    pub(super) fn decode(payload: &[u8]) -> Result<Self, LinuxError> {
        let info = RtMsg::read(payload)?;
        if info.tos != 0 || info.flags != 0 {
            return Err(LinuxError::EOPNOTSUPP);
        }
        let attrs = RouteRequestAttrs::decode(info.family, parse_attrs(&payload[RtMsg::SIZE..])?)?;
        Ok(Self {
            family: info.family,
            dst_len: info.dst_len,
            src_len: info.src_len,
            table: attrs.table.unwrap_or(u32::from(info.table)),
            protocol: info.protocol,
            scope: info.scope,
            route_type: info.route_type,
            oif: InterfaceIndex::new(attrs.oif).ok(),
            dst: attrs.dst,
            src: attrs.src,
            gateway: attrs.gateway,
            prefsrc: attrs.prefsrc,
        })
    }

    pub(super) fn add(self, flags: RequestFlags) -> Result<(), LinuxError> {
        ensure_service_inited()?;
        let mut service = SERVICE.lock();
        let rule = route_rule_from_request(self)?;
        let existing = service
            .route_rules()
            .iter()
            .copied()
            .find(|route| same_route_key(*route, &self));
        if let Some(existing) = existing {
            if flags.is_exclusive() || !flags.replaces() || !existing.is_configured() {
                return Err(LinuxError::EEXIST);
            }
            service.replace_route_rule(existing, rule)?;
        } else if !flags.creates() {
            return Err(LinuxError::ENOENT);
        } else {
            service.add_route_rule(rule)?;
        }
        Ok(())
    }

    pub(super) fn delete(self) -> Result<(), LinuxError> {
        validate_route_request(self)?;
        if normalize_route_table(self.table) != u32::from(ROUTE_TABLE_MAIN)
            || normalize_route_type(self.route_type) != ROUTE_TYPE_UNICAST
        {
            return Err(LinuxError::EOPNOTSUPP);
        }
        ensure_service_inited()?;
        let mut service = SERVICE.lock();
        if let Some(ifindex) = self.oif {
            let dev = ifindex.device_index();
            if !service.has_device(dev) {
                return Err(LinuxError::ENODEV);
            }
        }
        let removed_route = service
            .route_rules()
            .iter()
            .copied()
            .find(|route| route.is_configured() && route_delete_matches(*route, &self))
            .ok_or(LinuxError::ESRCH)?;
        service.remove_route_rule(removed_route)?;
        Ok(())
    }
}

impl NeighRequest {
    fn decode(payload: &[u8]) -> Result<Self, LinuxError> {
        let info = NdMsg::read(payload)?;
        if info.neigh_type != 0 {
            return Err(LinuxError::EOPNOTSUPP);
        }
        let attrs = NeighAttrs::decode(info.family, parse_attrs(&payload[NdMsg::SIZE..])?)?;
        Ok(Self {
            family: info.family,
            ifindex: InterfaceIndex::from_signed(info.ifindex)?,
            state: info.state,
            flags: info.flags,
            dst: attrs.dst.ok_or(LinuxError::EINVAL)?,
            lladdr: attrs.lladdr,
        })
    }

    pub(super) fn add(self, flags: RequestFlags) -> Result<(), LinuxError> {
        if self.family != wire_route::FAMILY_IPV4 {
            return Err(LinuxError::EAFNOSUPPORT);
        }
        ensure_service_inited()?;
        let mut service = SERVICE.lock();
        ensure_link_exists(&service, self.ifindex)?;
        let update = neighbor_update_from_request(self)?;
        let existing = service.has_neighbor(update.dev, update.dst);
        if existing && (flags.is_exclusive() || !flags.replaces()) {
            return Err(LinuxError::EEXIST);
        }
        if !existing && !flags.creates() {
            return Err(LinuxError::ENOENT);
        }
        service.apply_neighbor_update(update)
    }
}

fn ensure_link_exists(service: &Service, ifindex: InterfaceIndex) -> Result<(), LinuxError> {
    service
        .has_device(ifindex.device_index())
        .then_some(())
        .ok_or(LinuxError::ENODEV)
}

fn same_addr_entry(entry: Ipv4AddrEntry, request: &AddrRequest) -> bool {
    entry.dev as u32 + 1 == request.index.get()
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
        && request
            .oif
            .is_none_or(|ifindex| route.dev as u32 + 1 == ifindex.get())
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
        dev: request.ifindex.device_index(),
        dst: request.dst,
        state,
    })
}

impl LinkAttrs {
    fn decode(attrs: Vec<NlAttr<'_>>) -> Result<Self, LinuxError> {
        let mut parsed = Self::default();
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
}

impl AddrAttrs {
    fn decode(family: u8, attrs: Vec<NlAttr<'_>>) -> Result<Self, LinuxError> {
        let mut parsed = Self::default();
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
}

impl RouteRequestAttrs {
    fn decode(family: u8, attrs: Vec<NlAttr<'_>>) -> Result<Self, LinuxError> {
        let mut parsed = Self::default();
        for attr in attrs {
            match attr.kind {
                wire_route::attr::DST => {
                    parsed.dst = Some(parse_ip_by_family(family, attr.payload)?)
                }
                wire_route::attr::OIF => parsed.oif = read_u32_payload(attr.payload)?,
                wire_route::attr::GATEWAY => {
                    parsed.gateway = Some(parse_ip_by_family(family, attr.payload)?)
                }
                wire_route::attr::SRC => {
                    parsed.src = Some(parse_ip_by_family(family, attr.payload)?)
                }
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
}

impl NeighAttrs {
    fn decode(family: u8, attrs: Vec<NlAttr<'_>>) -> Result<Self, LinuxError> {
        let mut parsed = Self::default();
        for attr in attrs {
            match attr.kind {
                wire_neigh::attr::DST => {
                    parsed.dst = Some(parse_ip_by_family(family, attr.payload)?)
                }
                wire_neigh::attr::LLADDR => parsed.lladdr = Some(parse_mac(attr.payload)?),
                _ => return Err(LinuxError::EOPNOTSUPP),
            }
        }
        Ok(parsed)
    }
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

fn ack_packets(request: &[u8], flags: RequestFlags) -> Vec<NetlinkPacket> {
    if !flags.requires_ack() {
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

fn route_rule_from_request(request: RouteRequest) -> Result<Rule, LinuxError> {
    validate_route_request(request)?;
    let filter = match request
        .dst
        .unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED))
    {
        IpAddress::Ipv4(addr) => IpCidr::Ipv4(Ipv4Cidr::new(addr, request.dst_len).network()),
        IpAddress::Ipv6(_) => return Err(LinuxError::EAFNOSUPPORT),
    };
    let dev = request.oif.ok_or(LinuxError::EOPNOTSUPP)?.device_index();
    let table =
        u8::try_from(normalize_route_table(request.table)).map_err(|_| LinuxError::EOPNOTSUPP)?;

    Ok(Rule::with_route_attrs(
        filter,
        request.gateway,
        dev,
        RouterRouteAttrs {
            table,
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
