// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Routing table and route selection.
use alloc::{boxed::Box, collections::VecDeque, string::String, vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use ktime_types::MonotonicInstant;
use smoltcp::{
    phy::{DeviceCapabilities, Medium},
    time::Instant,
    wire::IpAddress as SmoltcpIpAddress,
};

use super::{
    ipv4::{self, Ipv4Error, Ipv4Header},
    ipv6,
};
pub(crate) use crate::netlink::{
    RT_SCOPE_HOST as ROUTE_SCOPE_HOST, RT_SCOPE_LINK as ROUTE_SCOPE_LINK,
    RT_SCOPE_UNIVERSE as ROUTE_SCOPE_UNIVERSE, RT_TABLE_LOCAL as ROUTE_TABLE_LOCAL,
    RT_TABLE_MAIN as ROUTE_TABLE_MAIN, RTN_LOCAL as ROUTE_TYPE_LOCAL,
    RTN_UNICAST as ROUTE_TYPE_UNICAST, RTPROT_BOOT as ROUTE_PROTOCOL_BOOT,
    RTPROT_KERNEL as ROUTE_PROTOCOL_KERNEL,
};
use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::{SOCKET_BUFFER_SIZE, STANDARD_MTU},
    device::{LinkConfigUpdate, LinkKind, LinkSendSnapshot, LinkSnapshot, NetDevice},
    ip::{IpAddress, IpCidr, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr},
    netlink::{
        RtnetlinkState,
        wire::route::{FAMILY_IPV4, FAMILY_IPV6},
    },
};

const CONTROL_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;
const DATA_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AddressRouteKind {
    Local,
    Connected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleSource {
    DevicePrimary,
    Fixed(IpAddress),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleOrigin {
    Configured,
    Ipv4Address {
        owner_dev: usize,
        addr: Ipv4Cidr,
        kind: AddressRouteKind,
    },
    Ipv6Address {
        owner_dev: usize,
        addr: Ipv6Cidr,
        kind: AddressRouteKind,
    },
}

impl RuleOrigin {
    fn is_local(self) -> bool {
        matches!(
            self,
            Self::Ipv4Address {
                kind: AddressRouteKind::Local,
                ..
            } | Self::Ipv6Address {
                kind: AddressRouteKind::Local,
                ..
            }
        )
    }

    fn is_ipv4_address(self) -> bool {
        matches!(self, Self::Ipv4Address { .. })
    }

    fn is_ipv6_address(self) -> bool {
        matches!(self, Self::Ipv6Address { .. })
    }

    fn is_owned_by_device(self, dev: usize) -> bool {
        match self {
            Self::Ipv4Address { owner_dev, .. } | Self::Ipv6Address { owner_dev, .. } => {
                owner_dev == dev
            }
            Self::Configured => false,
        }
    }

    fn renumber_after_device_removal(&mut self, removed: usize) {
        match self {
            Self::Ipv4Address { owner_dev, .. } | Self::Ipv6Address { owner_dev, .. }
                if *owner_dev > removed =>
            {
                *owner_dev -= 1;
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub filter: IpCidr,
    pub via: Option<IpAddress>,
    pub dev: usize,
    pub src: IpAddress,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    pub prefsrc: Option<IpAddress>,
    source: RuleSource,
    origin: RuleOrigin,
}

impl Rule {
    pub fn new(filter: IpCidr, via: Option<IpAddress>, dev: usize, src: IpAddress) -> Self {
        Self {
            filter,
            via,
            dev,
            src,
            table: ROUTE_TABLE_MAIN,
            protocol: ROUTE_PROTOCOL_BOOT,
            scope: ROUTE_SCOPE_UNIVERSE,
            route_type: ROUTE_TYPE_UNICAST,
            prefsrc: Some(src),
            source: RuleSource::Fixed(src),
            origin: RuleOrigin::Configured,
        }
    }

    pub fn with_route_attrs(
        filter: IpCidr,
        via: Option<IpAddress>,
        dev: usize,
        src: IpAddress,
        attrs: RouteAttrs,
    ) -> Self {
        Self {
            filter,
            via,
            dev,
            src,
            table: attrs.table,
            protocol: attrs.protocol,
            scope: attrs.scope,
            route_type: attrs.route_type,
            prefsrc: attrs.prefsrc,
            source: attrs
                .prefsrc
                .map_or(RuleSource::DevicePrimary, RuleSource::Fixed),
            origin: RuleOrigin::Configured,
        }
    }

    fn for_address_route(
        filter: IpCidr,
        dev: usize,
        source: IpAddress,
        origin: RuleOrigin,
        scope: u8,
        table: u8,
        route_type: u8,
    ) -> Self {
        Self {
            filter,
            via: None,
            dev,
            src: source,
            table,
            protocol: ROUTE_PROTOCOL_KERNEL,
            scope,
            route_type,
            prefsrc: Some(source),
            source: RuleSource::Fixed(source),
            origin,
        }
    }

    fn for_ipv4_address(
        filter: Ipv4Cidr,
        dev: usize,
        owner: Ipv4AddrEntry,
        kind: AddressRouteKind,
        scope: u8,
    ) -> Self {
        Self::for_address_route(
            filter.into(),
            dev,
            IpAddress::Ipv4(owner.addr.address()),
            RuleOrigin::Ipv4Address {
                owner_dev: owner.dev,
                addr: owner.addr,
                kind,
            },
            scope,
            ROUTE_TABLE_MAIN,
            ROUTE_TYPE_UNICAST,
        )
    }

    fn for_ipv6_address(
        filter: Ipv6Cidr,
        dev: usize,
        owner: Ipv6AddrEntry,
        kind: AddressRouteKind,
        scope: u8,
    ) -> Self {
        let (table, route_type) = match kind {
            AddressRouteKind::Local => (ROUTE_TABLE_LOCAL, ROUTE_TYPE_LOCAL),
            AddressRouteKind::Connected => (ROUTE_TABLE_MAIN, ROUTE_TYPE_UNICAST),
        };
        Self::for_address_route(
            filter.into(),
            dev,
            IpAddress::Ipv6(owner.addr.address()),
            RuleOrigin::Ipv6Address {
                owner_dev: owner.dev,
                addr: owner.addr,
                kind,
            },
            scope,
            table,
            route_type,
        )
    }

    pub(crate) fn preferred_source(self) -> Option<IpAddress> {
        match self.source {
            RuleSource::DevicePrimary => None,
            RuleSource::Fixed(source) => Some(source),
        }
    }

    pub(crate) fn is_ipv6_address_derived(&self) -> bool {
        self.origin.is_ipv6_address()
    }

    pub(crate) fn ifindex(self) -> u32 {
        self.dev as u32 + 1
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RouteAttrs {
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    pub prefsrc: Option<IpAddress>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv4AddrEntry {
    pub dev: usize,
    pub addr: Ipv4Cidr,
    pub scope: u8,
}

impl Ipv4AddrEntry {
    pub(crate) fn ifindex(self) -> u32 {
        self.dev as u32 + 1
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ipv6AddrEntry {
    pub dev: usize,
    pub addr: Ipv6Cidr,
    pub scope: u8,
}

impl Ipv6AddrEntry {
    pub(crate) fn ifindex(self) -> u32 {
        self.dev as u32 + 1
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AddrSnapshot<E> {
    pub entry: E,
    pub label: String,
}

pub(crate) type Ipv4AddrSnapshot = AddrSnapshot<Ipv4AddrEntry>;
pub(crate) type Ipv6AddrSnapshot = AddrSnapshot<Ipv6AddrEntry>;

#[derive(Debug)]
pub(crate) struct DeviceRemoval {
    pub(crate) ifindex: u32,
    pub(crate) orphaned_ipv4_sources: Vec<Ipv4Address>,
}

trait DeviceBound {
    fn device(&self) -> usize;
    fn set_device(&mut self, device: usize);
}

impl DeviceBound for Ipv4AddrEntry {
    fn device(&self) -> usize {
        self.dev
    }

    fn set_device(&mut self, device: usize) {
        self.dev = device;
    }
}

impl DeviceBound for Ipv6AddrEntry {
    fn device(&self) -> usize {
        self.dev
    }

    fn set_device(&mut self, device: usize) {
        self.dev = device;
    }
}

fn retain_and_renumber_devices<T: DeviceBound>(entries: &mut Vec<T>, removed: usize) {
    entries.retain(|entry| entry.device() != removed);
    for entry in entries {
        if entry.device() > removed {
            entry.set_device(entry.device() - 1);
        }
    }
}

fn first_entry_for_ifindex<T: DeviceBound>(entries: &[T], ifindex: u32) -> Option<&T> {
    let dev = ifindex.checked_sub(1)? as usize;
    entries.iter().find(|entry| entry.device() == dev)
}

// TODO(mivik): optimize
pub struct RouteTable {
    rules: Vec<Rule>,
}
impl RouteTable {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn add_rule(&mut self, rule: Rule) {
        let rank = (rule.filter.prefix_len(), rule.origin.is_local() as u8);
        let idx = self.rules.partition_point(|existing| {
            let existing_rank = (
                existing.filter.prefix_len(),
                existing.origin.is_local() as u8,
            );
            existing_rank >= rank
        });
        self.rules.insert(idx, rule);
    }

    pub fn lookup(&self, dst: &IpAddress) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.filter.contains_addr(dst))
    }

    pub fn lookup_on_device(&self, dst: &IpAddress, dev: usize) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.dev == dev && rule.filter.contains_addr(dst))
    }

    pub fn clear(&mut self) {
        self.rules.clear();
    }

    fn remove_ipv4_address_routes(&mut self) {
        self.rules.retain(|rule| !rule.origin.is_ipv4_address());
    }

    fn remove_ipv6_address_routes(&mut self) {
        self.rules.retain(|rule| !rule.origin.is_ipv6_address());
    }

    fn remove_configured_routes_with_source(&mut self, source: IpAddress) {
        self.rules.retain(|rule| {
            !matches!(rule.origin, RuleOrigin::Configured)
                || rule.preferred_source() != Some(source)
        });
    }

    pub fn remove_device(&mut self, dev: usize) {
        self.rules
            .retain(|rule| rule.dev != dev && !rule.origin.is_owned_by_device(dev));
        for rule in &mut self.rules {
            if rule.dev > dev {
                rule.dev -= 1;
            }
            rule.origin.renumber_after_device_removal(dev);
        }
    }

    pub(crate) fn rules(&self) -> &[Rule] {
        &self.rules
    }
}

pub struct Router {
    rx_queue: VecDeque<PacketBuf>,
    control_tx_queue: VecDeque<PacketBuf>,
    data_tx_queue: VecDeque<PacketBuf>,
    next_rx_device: usize,
    ipv4_addrs: Vec<Ipv4AddrEntry>,
    ipv6_addrs: Vec<Ipv6AddrEntry>,
    // Local delivery keeps the ingress interface so scoped IPv6 sockets match it.
    local_rx_queue: VecDeque<PacketBuf>,
    next_ipv4_identification: u16,
    pub(crate) devices: Vec<Box<dyn NetDevice>>,
    pub(crate) table: RouteTable,
    effective_mtu: usize,
}

pub(crate) struct RxDrain {
    pub work_done: usize,
    pub has_more: bool,
}
impl Router {
    pub fn new() -> Self {
        Self {
            rx_queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            control_tx_queue: VecDeque::with_capacity(CONTROL_TX_QUEUE_SIZE),
            data_tx_queue: VecDeque::with_capacity(DATA_TX_QUEUE_SIZE),
            next_rx_device: 0,
            ipv4_addrs: Vec::new(),
            ipv6_addrs: Vec::new(),
            local_rx_queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            next_ipv4_identification: 1,
            devices: Vec::new(),
            table: RouteTable::new(),
            effective_mtu: STANDARD_MTU,
        }
    }

    pub fn route_mtu(&self, dst: &IpAddress) -> Option<usize> {
        let rule = self.table.lookup(dst)?;
        self.devices
            .get(rule.dev)
            .map(|device| device.mtu().min(u16::MAX as usize))
    }

    /// Returns the source address for an output route with an active device.
    ///
    /// # Errors
    ///
    /// Returns `ENETUNREACH` when the route or its device is absent, or when
    /// the selected device is administratively down.
    pub fn output_route_source(&self, dst: &IpAddress) -> KResult<IpAddress> {
        let rule = self
            .table
            .lookup(dst)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        let device = self
            .devices
            .get(rule.dev)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        if !device.is_link_up() {
            return Err(LinuxError::ENETUNREACH.into());
        }
        match rule.source {
            RuleSource::Fixed(source) => Ok(source),
            RuleSource::DevicePrimary => self
                .select_ipv4_source(rule.dev, &rule.via.unwrap_or(*dst), rule.scope)
                .map(IpAddress::Ipv4)
                .ok_or(KError::from(LinuxError::ENETUNREACH)),
        }
    }

    /// Resolves the IPv6 source address and output interface for a datagram.
    ///
    /// Unicast destinations still require a matching route, including when the
    /// socket already bound a source. Multicast destinations do not consult the
    /// unicast table; Linux installs `ff00::/8` via `addrconf_add_mroute`, while
    /// knet selects the scoped device and a unicast source on that device.
    pub fn ipv6_output_source(
        &self,
        dst: &IpAddress,
        bound_src: Option<IpAddress>,
        scope_id: u32,
    ) -> KResult<(IpAddress, i32)> {
        let IpAddress::Ipv6(dst_v6) = *dst else {
            return Err(LinuxError::EAFNOSUPPORT.into());
        };
        if dst_v6.is_multicast() {
            return self.ipv6_multicast_output_source(bound_src, scope_id);
        }

        let rule = if scope_id == 0 {
            self.table.lookup(dst)
        } else {
            self.table
                .lookup_on_device(dst, scope_id.saturating_sub(1) as usize)
        }
        .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        self.require_output_device_up(rule.dev)?;
        let source = self.ipv6_bound_or_route_source(bound_src, rule.src)?;
        Ok((source, rule.ifindex() as i32))
    }

    fn ipv6_multicast_output_source(
        &self,
        bound_src: Option<IpAddress>,
        scope_id: u32,
    ) -> KResult<(IpAddress, i32)> {
        let dev = if scope_id != 0 {
            let dev = scope_id.saturating_sub(1) as usize;
            self.require_output_device_up(dev)?;
            dev
        } else if let Some(IpAddress::Ipv6(src)) = bound_src {
            let dev = self
                .ipv6_addrs
                .iter()
                .find(|entry| entry.addr.address() == src)
                .map(|entry| entry.dev)
                .ok_or(KError::from(LinuxError::ENETUNREACH))?;
            self.require_output_device_up(dev)?;
            dev
        } else {
            // Link-local multicast needs an output interface, matching Linux
            // `ip6_dst_lookup` when `flowi6_oif` is unset.
            return Err(LinuxError::ENETUNREACH.into());
        };
        let route_source = self
            .ipv6_unicast_source_on_device(dev)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        let source = self.ipv6_bound_or_route_source(bound_src, route_source)?;
        Ok((source, dev as i32 + 1))
    }

    fn ipv6_bound_or_route_source(
        &self,
        bound_src: Option<IpAddress>,
        route_source: IpAddress,
    ) -> KResult<IpAddress> {
        match bound_src {
            Some(addr) if addr.is_unspecified() || addr.is_multicast() => {
                Err(LinuxError::EADDRNOTAVAIL.into())
            }
            Some(addr) => Ok(addr),
            None => Ok(route_source),
        }
    }

    fn ipv6_unicast_source_on_device(&self, dev: usize) -> Option<IpAddress> {
        let mut fallback = None;
        for entry in &self.ipv6_addrs {
            if entry.dev != dev {
                continue;
            }
            let addr = entry.addr.address();
            if !addr.is_unicast() {
                continue;
            }
            if addr.is_link_local() {
                return Some(IpAddress::Ipv6(addr));
            }
            fallback.get_or_insert(IpAddress::Ipv6(addr));
        }
        fallback
    }

    fn require_output_device_up(&self, dev: usize) -> KResult {
        let device = self
            .devices
            .get(dev)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        if !device.is_link_up() {
            return Err(LinuxError::ENETUNREACH.into());
        }
        Ok(())
    }

    pub(crate) fn ipv6_output_mtu(&self, dst: &IpAddress, ifindex: i32) -> Option<usize> {
        self.route_mtu(dst).or_else(|| {
            Self::device_index(ifindex)
                .and_then(|dev| self.devices.get(dev))
                .map(|device| device.mtu().min(u16::MAX as usize))
        })
    }

    pub fn can_enqueue_tx_packet(&self) -> bool {
        self.data_tx_queue.len() < DATA_TX_QUEUE_SIZE
    }

    pub(crate) fn available_tx_packet_slots(&self) -> usize {
        DATA_TX_QUEUE_SIZE.saturating_sub(self.data_tx_queue.len())
    }

    pub fn can_enqueue_tx_packets(&self, packet_count: usize) -> bool {
        self.data_tx_queue
            .len()
            .checked_add(packet_count)
            .is_some_and(|len| len <= DATA_TX_QUEUE_SIZE)
    }

    pub fn queue_ipv4_packet(&mut self, packet: Vec<u8>) -> KResult {
        let packets = self.fragment_ipv4_packet_for_output(packet)?;
        if !self.can_enqueue_tx_packets(packets.len()) {
            return Err(KError::WouldBlock);
        }

        self.data_tx_queue.extend(
            packets
                .into_iter()
                .map(|packet| PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack)),
        );
        Ok(())
    }

    pub(crate) fn push_data_tx_packet(&mut self, packet: PacketBuf) {
        self.data_tx_queue.push_back(packet);
    }

    pub(crate) fn local_ipv6_device_index(&self, dst_addr: Ipv6Address) -> Option<usize> {
        self.ipv6_addrs
            .iter()
            .find(|entry| entry.addr.address() == dst_addr)
            .map(|entry| entry.dev)
    }

    pub(crate) fn try_enqueue_local_ipv6_packet(&mut self, packet: PacketBuf) -> bool {
        if self.local_rx_queue.len() >= SOCKET_BUFFER_SIZE {
            warn!("Local IPv6 receive queue is full, dropping packet");
            return false;
        }
        self.local_rx_queue.push_back(packet);
        true
    }

    pub fn queue_ipv6_packet_on_device(&mut self, packet: Vec<u8>, ifindex: i32) -> KResult {
        ipv6::queue_ipv6_packet(self, packet, ifindex)
    }

    pub fn add_rule(&mut self, rule: Rule) {
        self.table.add_rule(rule);
    }

    pub fn add_device(&mut self, device: Box<dyn NetDevice>) -> usize {
        self.devices.push(device);
        self.sync_device_ipv4_addrs();
        self.sync_device_ipv6_addrs();
        self.recompute_effective_mtu();
        self.devices.len() - 1
    }

    pub(crate) fn remove_device_by_model_id(
        &mut self,
        id: kdevice::DeviceId,
    ) -> Option<DeviceRemoval> {
        let pos = self
            .devices
            .iter()
            .position(|device| device.device_id() == Some(id))?;
        let ifindex = pos as u32 + 1;
        let removed_sources: Vec<_> = self
            .ipv4_addrs
            .iter()
            .filter_map(|entry| (entry.dev == pos).then_some(entry.addr.address()))
            .collect();
        self.devices.remove(pos);
        self.table.remove_device(pos);
        retain_and_renumber_devices(&mut self.ipv4_addrs, pos);
        retain_and_renumber_devices(&mut self.ipv6_addrs, pos);
        let mut orphaned_ipv4_sources = Vec::new();
        for source in removed_sources {
            if !self.has_ipv4_address(source) && !orphaned_ipv4_sources.contains(&source) {
                self.table
                    .remove_configured_routes_with_source(source.into());
                for device in &mut self.devices {
                    device.remove_pending_ipv4_source(source);
                }
                orphaned_ipv4_sources.push(source);
            }
        }
        self.rebuild_ipv4_address_routes();
        self.rebuild_ipv6_address_routes();
        self.sync_device_ipv4_addrs();
        self.sync_device_ipv6_addrs();
        self.adjust_rx_device_cursor_after_remove(pos);
        self.recompute_effective_mtu();
        Some(DeviceRemoval {
            ifindex,
            orphaned_ipv4_sources,
        })
    }

    pub fn local_ipv4_addrs(&self) -> Vec<Ipv4Cidr> {
        self.ipv4_addrs.iter().map(|entry| entry.addr).collect()
    }

    pub fn local_ipv6_addrs(&self) -> Vec<Ipv6Cidr> {
        self.ipv6_addrs.iter().map(|entry| entry.addr).collect()
    }

    /// Returns the MTU advertised through the smoltcp device capabilities.
    pub fn effective_mtu(&self) -> usize {
        self.effective_mtu
    }

    pub fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.devices
            .iter()
            .enumerate()
            .map(|(dev, device)| device.link_snapshot(dev as i32 + 1))
            .collect()
    }

    pub fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        let dev_index = Self::device_index(ifindex)?;
        self.devices
            .get(dev_index)
            .map(|device| device.link_snapshot(ifindex))
    }

    pub fn has_device(&self, ifindex: i32) -> bool {
        Self::device_index(ifindex).is_some_and(|dev_index| self.devices.get(dev_index).is_some())
    }

    pub fn link_send_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSendSnapshot> {
        let dev_index = Self::device_index(ifindex)?;
        self.devices
            .get(dev_index)
            .map(|device| device.link_send_snapshot())
    }

    pub fn ipv4_addr_entries(&self) -> &[Ipv4AddrEntry] {
        &self.ipv4_addrs
    }

    pub fn ipv4_addr_snapshots(&self) -> Vec<Ipv4AddrSnapshot> {
        self.labeled_addr_snapshots(&self.ipv4_addrs)
    }

    pub fn add_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        self.validate_new_unicast_addr(entry.dev, entry.addr.address().is_unicast())?;
        if self
            .ipv4_addrs
            .iter()
            .any(|existing| existing.dev == entry.dev && existing.addr == entry.addr)
        {
            return Ok(());
        }
        if !self.has_iface_addr_capacity() {
            return Err(LinuxError::ENOBUFS);
        }

        self.ipv4_addrs.push(entry);
        self.rebuild_ipv4_address_routes();
        self.sync_device_ipv4_addrs();
        Ok(())
    }

    pub fn remove_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> bool {
        let old_len = self.ipv4_addrs.len();
        self.ipv4_addrs
            .retain(|existing| existing.dev != entry.dev || existing.addr != entry.addr);
        if self.ipv4_addrs.len() == old_len {
            return false;
        }

        let source = IpAddress::Ipv4(entry.addr.address());
        if !self.has_ipv4_address(entry.addr.address()) {
            self.table.remove_configured_routes_with_source(source);
            for device in &mut self.devices {
                device.remove_pending_ipv4_source(entry.addr.address());
            }
        }
        self.rebuild_ipv4_address_routes();
        self.sync_device_ipv4_addrs();
        true
    }

    pub fn has_ipv4_address(&self, address: Ipv4Address) -> bool {
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.addr.address() == address)
    }

    pub fn ipv6_addr_snapshots(&self) -> Vec<Ipv6AddrSnapshot> {
        self.labeled_addr_snapshots(&self.ipv6_addrs)
    }

    pub fn ipv6_address_route_snapshots(&self) -> Vec<Rule> {
        self.table
            .rules()
            .iter()
            .copied()
            .filter(Rule::is_ipv6_address_derived)
            .collect()
    }

    pub fn add_ipv6_addr(&mut self, entry: Ipv6AddrEntry) -> Result<(), LinuxError> {
        if entry.dev >= self.devices.len() {
            return Err(LinuxError::ENODEV);
        }
        let address = entry.addr.address();
        if !address.is_unicast() {
            return Err(LinuxError::EADDRNOTAVAIL);
        }
        if address.is_loopback() && self.devices[entry.dev].link_kind() != LinkKind::Loopback {
            return Err(LinuxError::EADDRNOTAVAIL);
        }
        if self
            .ipv6_addrs
            .iter()
            .any(|existing| existing.dev == entry.dev && existing.addr == entry.addr)
        {
            return Ok(());
        }
        if !self.has_iface_addr_capacity() {
            return Err(LinuxError::ENOBUFS);
        }

        self.ipv6_addrs.push(entry);
        self.rebuild_ipv6_address_routes();
        self.sync_device_ipv6_addrs();
        Ok(())
    }

    /// Applies a validated link update.
    ///
    /// Returns `Ok(true)` when the effective device MTU changed and the
    /// smoltcp interface must be rebuilt to refresh its cached capabilities.
    pub fn update_device_link(
        &mut self,
        ifindex: i32,
        update: LinkConfigUpdate,
    ) -> Result<bool, LinuxError> {
        let dev = Self::device_index(ifindex).ok_or(LinuxError::ENODEV)?;
        let device = self.devices.get(dev).ok_or(LinuxError::ENODEV)?;
        if let Some(name) = update.name.as_deref() {
            crate::device::validate_interface_name(name)?;
            if self
                .devices
                .iter()
                .enumerate()
                .any(|(other_dev, device)| other_dev != dev && device.name() == name)
            {
                return Err(LinuxError::EEXIST);
            }
        }
        if let Some(mtu) = update.mtu {
            device.link_kind().validate_mtu(mtu)?;
        }

        let is_mtu_changed = update.mtu.is_some_and(|mtu| mtu != device.mtu());
        let effective_mtu = self.effective_mtu;

        let LinkConfigUpdate { name, mtu, is_up } = update;
        let device = &mut self.devices[dev];
        if let Some(mtu) = mtu {
            device.set_mtu(mtu)?;
        }
        if let Some(name) = name {
            device.set_name(name);
        }
        if let Some(is_up) = is_up {
            device.set_link_up(is_up);
        }
        if is_mtu_changed {
            self.recompute_effective_mtu();
        }
        Ok(self.effective_mtu != effective_mtu)
    }

    fn device_index(ifindex: i32) -> Option<usize> {
        (ifindex > 0).then_some((ifindex - 1) as usize)
    }

    pub fn sync_netlink(&mut self, state: &RtnetlinkState) {
        self.sync_neighbors_from_netlink(state);
        self.replace_configured_routes(state);
        self.rebuild_ipv4_address_routes();
        self.rebuild_ipv6_address_routes();
        self.sync_device_ipv4_addrs();
        self.sync_device_ipv6_addrs();
    }

    fn sync_neighbors_from_netlink(&mut self, state: &RtnetlinkState) {
        for (dev_index, device) in self.devices.iter_mut().enumerate() {
            let ifindex = (dev_index as u32).saturating_add(1);
            let neighbors: Vec<_> = state
                .neighs
                .iter()
                .filter(|neigh| neigh.ifindex == ifindex)
                .filter_map(|neigh| {
                    let hardware_addr = neigh.lladdr?;
                    Some((super::from_smoltcp_ip_address(neigh.dst), hardware_addr))
                })
                .collect();
            device.sync_neighbors(&neighbors);
        }
    }

    fn replace_configured_routes(&mut self, state: &RtnetlinkState) {
        self.table.clear();
        for route in state.routes.iter().filter(|route| {
            (route.family == FAMILY_IPV4 || route.family == FAMILY_IPV6)
                && route.table == ROUTE_TABLE_MAIN
                && route.route_type == ROUTE_TYPE_UNICAST
        }) {
            let Some(rule) = self.configured_rule_from_netlink(route) else {
                continue;
            };
            self.table.add_rule(rule);
        }
    }

    fn configured_rule_from_netlink(&self, route: &crate::netlink::RouteState) -> Option<Rule> {
        let dev = route.oif.saturating_sub(1) as usize;
        if dev >= self.devices.len() {
            return None;
        }

        let filter = match (route.family, route.dst) {
            (FAMILY_IPV4, Some(SmoltcpIpAddress::Ipv4(addr))) => {
                IpCidr::Ipv4(Ipv4Cidr::new(addr.into(), route.dst_len))
            }
            (FAMILY_IPV4, None) => {
                IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, route.dst_len))
            }
            (FAMILY_IPV6, Some(SmoltcpIpAddress::Ipv6(addr))) => {
                IpCidr::Ipv6(Ipv6Cidr::new(addr.into(), route.dst_len))
            }
            (FAMILY_IPV6, None) => {
                IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Address::UNSPECIFIED, route.dst_len))
            }
            _ => return None,
        };
        let src = route
            .prefsrc
            .or_else(|| {
                if route.family == FAMILY_IPV4 {
                    self.first_ipv4_addr_for_device(route.oif)
                        .map(|addr| SmoltcpIpAddress::Ipv4(addr.address().into()))
                } else {
                    self.first_ipv6_addr_for_device(route.oif)
                        .map(|addr| SmoltcpIpAddress::Ipv6(addr.address().into()))
                }
            })
            .map(super::from_smoltcp_ip_address)
            .unwrap_or(filter.address());
        Some(Rule::with_route_attrs(
            filter,
            route.gateway.map(super::from_smoltcp_ip_address),
            dev,
            src,
            RouteAttrs {
                table: route.table,
                protocol: route.protocol,
                scope: route.scope,
                route_type: route.route_type,
                prefsrc: route.prefsrc.map(super::from_smoltcp_ip_address),
            },
        ))
    }

    fn first_ipv4_addr_for_device(&self, ifindex: u32) -> Option<Ipv4Cidr> {
        first_entry_for_ifindex(&self.ipv4_addrs, ifindex).map(|entry| entry.addr)
    }

    fn first_ipv6_addr_for_device(&self, ifindex: u32) -> Option<Ipv6Cidr> {
        first_entry_for_ifindex(&self.ipv6_addrs, ifindex).map(|entry| entry.addr)
    }

    fn select_ipv4_source(
        &self,
        dev: usize,
        next_hop: &IpAddress,
        route_scope: u8,
    ) -> Option<Ipv4Address> {
        let mut eligible = self
            .ipv4_addrs
            .iter()
            .filter(|entry| entry.dev == dev && entry.scope <= route_scope);
        let first = eligible.next()?;
        let selected = if first.addr.contains_addr(next_hop) {
            first
        } else {
            eligible
                .find(|entry| entry.addr.contains_addr(next_hop))
                .unwrap_or(first)
        };
        Some(selected.addr.address())
    }

    fn sync_device_ipv4_addrs(&mut self) {
        let local_addrs: Vec<_> = self.ipv4_addrs.iter().map(|entry| entry.addr).collect();
        let assigned_addrs: Vec<Vec<_>> = (0..self.devices.len())
            .map(|dev| {
                self.ipv4_addrs
                    .iter()
                    .filter_map(|entry| (entry.dev == dev).then_some(entry.addr))
                    .collect()
            })
            .collect();
        for (device, assigned_addrs) in self.devices.iter_mut().zip(assigned_addrs) {
            device.set_ipv4_addrs(&assigned_addrs, &local_addrs);
        }
    }

    fn sync_device_ipv6_addrs(&mut self) {
        let assigned_addrs: Vec<Vec<_>> = (0..self.devices.len())
            .map(|dev| {
                self.ipv6_addrs
                    .iter()
                    .filter_map(|entry| (entry.dev == dev).then_some(entry.addr))
                    .collect()
            })
            .collect();
        for (device, assigned_addrs) in self.devices.iter_mut().zip(assigned_addrs) {
            device.set_ipv6_addrs(&assigned_addrs);
        }
    }

    fn rebuild_ipv4_address_routes(&mut self) {
        let rules = self.ipv4_address_derived_rules();
        self.table.remove_ipv4_address_routes();
        for rule in rules {
            self.table.add_rule(rule);
        }
    }

    fn rebuild_ipv6_address_routes(&mut self) {
        let rules = self.ipv6_address_derived_rules();
        self.table.remove_ipv6_address_routes();
        for rule in rules {
            self.table.add_rule(rule);
        }
    }

    fn ipv4_address_derived_rules(&self) -> Vec<Rule> {
        let loopback_dev = self.loopback_device_index();
        let mut connected = Vec::new();
        let mut rules = Vec::new();
        for &entry in &self.ipv4_addrs {
            if let Some(loopback_dev) = loopback_dev {
                rules.push(Rule::for_ipv4_address(
                    Ipv4Cidr::new(entry.addr.address(), 32),
                    loopback_dev,
                    entry,
                    AddressRouteKind::Local,
                    ROUTE_SCOPE_HOST,
                ));
            }

            let network = entry.addr.network();
            if entry.addr.prefix_len() == 32
                || network.address().is_unspecified()
                || connected.contains(&(entry.dev, network))
            {
                continue;
            }
            connected.push((entry.dev, network));
            rules.push(Rule::for_ipv4_address(
                network,
                entry.dev,
                entry,
                AddressRouteKind::Connected,
                self.connected_route_scope(entry.dev),
            ));
        }
        rules
    }

    fn ipv6_address_derived_rules(&self) -> Vec<Rule> {
        let mut connected = Vec::new();
        let mut rules = Vec::new();
        for &entry in &self.ipv6_addrs {
            // Linux addrconf_f6i_alloc installs the /128 host route on the
            // address's own device in table local with RTN_LOCAL, not via lo.
            rules.push(Rule::for_ipv6_address(
                Ipv6Cidr::new(entry.addr.address(), 128),
                entry.dev,
                entry,
                AddressRouteKind::Local,
                ROUTE_SCOPE_UNIVERSE,
            ));

            let network = entry.addr.network();
            if entry.addr.prefix_len() == 128
                || network.address().is_unspecified()
                || connected.contains(&(entry.dev, network))
            {
                continue;
            }
            connected.push((entry.dev, network));
            // Linux addrconf_prefix_route puts on-link prefixes in table main.
            rules.push(Rule::for_ipv6_address(
                network,
                entry.dev,
                entry,
                AddressRouteKind::Connected,
                ROUTE_SCOPE_UNIVERSE,
            ));
        }
        rules
    }

    fn loopback_device_index(&self) -> Option<usize> {
        self.devices
            .iter()
            .position(|device| device.link_kind() == LinkKind::Loopback)
    }

    fn connected_route_scope(&self, dev: usize) -> u8 {
        if self.devices[dev].link_kind() == LinkKind::Loopback {
            ROUTE_SCOPE_HOST
        } else {
            ROUTE_SCOPE_LINK
        }
    }

    fn validate_new_unicast_addr(&self, dev: usize, is_unicast: bool) -> Result<(), LinuxError> {
        if dev >= self.devices.len() {
            return Err(LinuxError::ENODEV);
        }
        if !is_unicast {
            return Err(LinuxError::EINVAL);
        }
        Ok(())
    }

    fn has_iface_addr_capacity(&self) -> bool {
        self.ipv4_addrs.len() + self.ipv6_addrs.len() < smoltcp::config::IFACE_MAX_ADDR_COUNT
    }

    fn labeled_addr_snapshots<E: Copy + DeviceBound>(&self, entries: &[E]) -> Vec<AddrSnapshot<E>> {
        entries
            .iter()
            .filter_map(|entry| {
                self.devices.get(entry.device()).map(|device| AddrSnapshot {
                    entry: *entry,
                    label: String::from(device.name()),
                })
            })
            .collect()
    }

    fn recompute_effective_mtu(&mut self) {
        self.effective_mtu = self
            .devices
            .iter()
            .map(|device| device.mtu().min(u16::MAX as usize))
            .min()
            .unwrap_or(STANDARD_MTU);
    }

    pub fn drain_rx_budgeted_into(
        &mut self,
        timestamp: MonotonicInstant,
        budget: usize,
        packets: &mut Vec<PacketBuf>,
    ) -> RxDrain {
        packets.clear();
        if budget == 0 || (self.devices.is_empty() && self.local_rx_queue.is_empty()) {
            return RxDrain {
                work_done: 0,
                has_more: self.has_immediate_rx_work(),
            };
        }

        let mut work_done = 0;

        // Locally looped packets keep their ingress interface so scoped IPv6
        // sockets match the device that originated them.
        while work_done < budget {
            let Some(packet) = self.local_rx_queue.pop_front() else {
                break;
            };
            work_done += 1;
            packets.push(packet);
        }

        let device_count = self.devices.len();
        if self.next_rx_device >= device_count {
            self.next_rx_device = 0;
        }

        for _ in 0..device_count {
            let dev_index = self.next_rx_device;
            let ifindex = dev_index as i32 + 1;
            loop {
                if work_done >= budget {
                    self.next_rx_device = next_device_index(dev_index, device_count);
                    return RxDrain {
                        work_done,
                        has_more: self.has_immediate_rx_work(),
                    };
                }

                let packet = {
                    let dev = &mut self.devices[dev_index];
                    dev.poll_rx(ifindex, timestamp)
                };
                let Some(packet) = packet else {
                    break;
                };
                work_done += 1;
                packets.push(packet);
            }
            self.next_rx_device = next_device_index(dev_index, device_count);
        }

        RxDrain {
            work_done,
            has_more: false,
        }
    }

    pub fn enqueue_ingress_packets(&mut self, packets: &mut Vec<PacketBuf>) {
        debug_assert!(packets.len() <= self.ingress_capacity());
        self.rx_queue.extend(packets.drain(..));
    }

    pub fn ingress_capacity(&self) -> usize {
        SOCKET_BUFFER_SIZE.saturating_sub(self.rx_queue.len())
    }

    pub fn has_pending_ingress(&self) -> bool {
        !self.rx_queue.is_empty()
    }

    pub fn send_link_frame(&mut self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        let dev_index = Self::device_index(ifindex).ok_or(KError::InvalidInput)?;
        let dev = self
            .devices
            .get_mut(dev_index)
            .ok_or(KError::from(LinuxError::ENODEV))?;
        dev.send_link_frame(ifindex, frame)
    }

    pub fn dispatch_budgeted(
        &mut self,
        timestamp: MonotonicInstant,
        budget: usize,
    ) -> (usize, bool) {
        if budget == 0 {
            return (0, self.has_queued_tx_packets());
        }

        let mut work_done = 0;
        let mut poll_next = false;
        while work_done < budget {
            let Some(mut packet) = self.pop_tx_packet() else {
                break;
            };
            work_done += 1;
            packet.set_owner(PacketOwner::DeviceTx);
            let Some(ip_packet) = packet.network_packet() else {
                continue;
            };
            match ipv4::ip_version(ip_packet) {
                Some(4) => {
                    poll_next |= self.dispatch_ipv4_packet(packet, timestamp);
                }
                Some(6) => {
                    poll_next |= ipv6::dispatch_ipv6_packet(self, packet, timestamp);
                }
                _ => debug!("Dropping packet with invalid IP version"),
            }
        }
        (work_done, poll_next || self.has_queued_tx_packets())
    }

    fn dispatch_ipv4_packet(&mut self, mut packet: PacketBuf, timestamp: MonotonicInstant) -> bool {
        let header = match Ipv4Header::prepare_output_packet(&mut packet) {
            Ok(header) => header,
            Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return false,
        };

        let src_addr = IpAddress::Ipv4(header.src_addr());
        let dst_addr = IpAddress::Ipv4(header.dst_addr());
        if header.dst_addr().is_broadcast() {
            if !self.is_valid_ipv4_broadcast_source(header.src_addr()) {
                warn!("Dropping IPv4 broadcast packet with source {}", src_addr);
                return false;
            }

            let device_count = self.devices.len();
            let Some((last_device, preceding_devices)) = self.devices.split_last_mut() else {
                return false;
            };

            let mut poll_next = false;
            for (dev_index, dev) in preceding_devices.iter_mut().enumerate() {
                poll_next |= dev.send_ip_packet(
                    dev_index as i32 + 1,
                    dst_addr,
                    src_addr,
                    packet.clone(),
                    timestamp,
                );
            }
            poll_next |= last_device.send_ip_packet(
                device_count as i32,
                dst_addr,
                src_addr,
                packet,
                timestamp,
            );
            return poll_next;
        }

        let Some(rule) = self.table.lookup(&dst_addr) else {
            warn!("No route found for destination: {}", dst_addr);
            return false;
        };
        if !self.is_local_ipv4_source(header.src_addr()) {
            warn!(
                "Dropping IPv4 packet with non-local source {} routed via {}",
                src_addr, rule.src
            );
            return false;
        }

        let next_hop = rule.via.unwrap_or(dst_addr);
        let dev = &mut self.devices[rule.dev];
        dev.send_ip_packet(rule.dev as i32 + 1, next_hop, src_addr, packet, timestamp)
    }

    fn fragment_ipv4_packet_for_output(&mut self, packet: Vec<u8>) -> KResult<Vec<Vec<u8>>> {
        let header = Ipv4Header::parse_output(&packet).map_err(|_| KError::InvalidInput)?;
        let dst_addr = IpAddress::Ipv4(header.dst_addr());
        let Some(mtu) = self.route_mtu(&dst_addr) else {
            return Ok(vec![packet]);
        };
        if packet.len() <= mtu {
            return Ok(vec![packet]);
        }

        let identification = self.next_ipv4_identification;
        self.next_ipv4_identification = self.next_ipv4_identification.wrapping_add(1).max(1);
        ipv4::fragment_output_packet(&packet, mtu, identification).map_err(|err| match err {
            ipv4::Ipv4FragmentError::Malformed => KError::InvalidInput,
            ipv4::Ipv4FragmentError::DontFragment | ipv4::Ipv4FragmentError::MtuTooSmall => {
                LinuxError::EMSGSIZE.into()
            }
        })
    }

    fn is_local_ipv4_source(&self, src_addr: crate::ip::Ipv4Address) -> bool {
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.addr.address() == src_addr)
    }

    fn is_valid_ipv4_broadcast_source(&self, src_addr: crate::ip::Ipv4Address) -> bool {
        src_addr.is_unspecified() || self.is_local_ipv4_source(src_addr)
    }

    pub fn queue_control_ip_packet(&mut self, packet: Vec<u8>) -> KResult {
        if self.control_tx_queue.len() >= CONTROL_TX_QUEUE_SIZE {
            return Err(KError::WouldBlock);
        }
        let owner = if matches!(ipv4::ip_version(&packet), Some(6)) {
            PacketOwner::Ipv6Stack
        } else {
            PacketOwner::Ipv4Stack
        };
        self.control_tx_queue
            .push_back(PacketBuf::from_ip_packet_vec(0, packet, owner));
        Ok(())
    }

    fn pop_tx_packet(&mut self) -> Option<PacketBuf> {
        self.control_tx_queue
            .pop_front()
            .or_else(|| self.data_tx_queue.pop_front())
    }

    fn has_queued_tx_packets(&self) -> bool {
        !self.control_tx_queue.is_empty() || !self.data_tx_queue.is_empty()
    }

    pub(crate) fn has_immediate_rx_work(&self) -> bool {
        !self.local_rx_queue.is_empty() || self.devices.iter().any(|device| device.has_rx_work())
    }

    fn adjust_rx_device_cursor_after_remove(&mut self, removed_index: usize) {
        if self.devices.is_empty() {
            self.next_rx_device = 0;
        } else if removed_index < self.next_rx_device {
            self.next_rx_device -= 1;
        } else if self.next_rx_device >= self.devices.len() {
            self.next_rx_device = 0;
        }
    }
}

fn next_device_index(dev_index: usize, device_count: usize) -> usize {
    debug_assert!(device_count > 0);
    (dev_index + 1) % device_count
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, vec, vec::Vec};

    use ktime_types::MonotonicInstant;
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::device::LoopbackDevice;

    fn router_with_ready_loopback_packet() -> Router {
        let mut router = Router::new();
        let loopback = router.add_device(Box::new(LoopbackDevice::new()));
        let packet = PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::Ipv4Stack);
        let _ = router.devices[loopback].send_ip_packet(
            1,
            IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
            IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
            packet,
            MonotonicInstant::ORIGIN,
        );
        router
    }

    #[def_test]
    fn exact_rx_budget_does_not_report_empty_device() {
        let mut router = router_with_ready_loopback_packet();
        let mut packets = Vec::new();

        let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);

        assert_eq!(drain.work_done, 1);
        assert!(!drain.has_more);
    }

    #[def_test]
    fn zero_rx_budget_reports_ready_device() {
        let mut router = router_with_ready_loopback_packet();
        let mut packets = Vec::new();

        let drain = router.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 0, &mut packets);

        assert_eq!(drain.work_done, 0);
        assert!(drain.has_more);
    }

    #[def_test]
    fn control_dispatch_does_not_free_data_tx_capacity() {
        let mut router = Router::new();
        router
            .control_tx_queue
            .push_back(PacketBuf::from_ip_packet_vec(
                0,
                vec![0; 20],
                PacketOwner::Ipv4Stack,
            ));
        let capacity_before = router.available_tx_packet_slots();

        let (work_done, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);

        assert_eq!(work_done, 1);
        assert_eq!(router.available_tx_packet_slots(), capacity_before);
    }

    #[def_test]
    fn data_dispatch_frees_data_tx_capacity() {
        let mut router = Router::new();
        router
            .data_tx_queue
            .push_back(PacketBuf::from_ip_packet_vec(
                0,
                vec![0; 20],
                PacketOwner::Ipv4Stack,
            ));
        let capacity_before = router.available_tx_packet_slots();

        let (work_done, _) = router.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);

        assert_eq!(work_done, 1);
        assert_eq!(router.available_tx_packet_slots(), capacity_before + 1);
    }

    #[def_test]
    fn ipv4_address_installs_local_and_connected_routes() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
        };

        router.add_ipv4_addr(entry).unwrap();

        let local = router
            .table
            .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)))
            .unwrap();
        assert_eq!(local.filter.prefix_len(), 32);
        assert_eq!(local.dev, dev);
        assert_eq!(local.scope, ROUTE_SCOPE_HOST);
        assert_eq!(local.protocol, ROUTE_PROTOCOL_KERNEL);

        let connected = router
            .table
            .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8)))
            .unwrap();
        assert_eq!(connected.filter.prefix_len(), 24);
        assert_eq!(connected.dev, dev);
        assert_eq!(connected.scope, ROUTE_SCOPE_HOST);
        assert_eq!(connected.protocol, ROUTE_PROTOCOL_KERNEL);
    }

    #[def_test]
    fn ipv4_address_capacity_matches_smoltcp_interface_limit() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));

        for index in 0..smoltcp::config::IFACE_MAX_ADDR_COUNT {
            router
                .add_ipv4_addr(Ipv4AddrEntry {
                    dev,
                    addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, index as u8 + 1), 32),
                    scope: ROUTE_SCOPE_HOST,
                })
                .unwrap();
        }

        assert_eq!(
            router.add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 1), 32),
                scope: ROUTE_SCOPE_HOST,
            }),
            Err(LinuxError::ENOBUFS)
        );
    }

    #[def_test]
    fn deleting_ipv4_address_removes_automatic_routes() {
        let mut router = Router::new();
        let dev = router.add_device(Box::new(LoopbackDevice::new()));
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
        };

        router.add_ipv4_addr(entry).unwrap();
        assert!(
            router
                .table
                .lookup(&IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 7)))
                .is_some()
        );

        assert!(router.remove_ipv4_addr(entry));
        assert!(
            router
                .table
                .lookup(&IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 7)))
                .is_none()
        );
    }

    #[def_test]
    fn ipv6_address_installs_local_and_connected_routes() {
        let mut router = Router::new();
        let lo = router.add_device(Box::new(LoopbackDevice::new()));
        let link_local = Ipv6Address::link_local_from_mac([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);

        router
            .add_ipv6_addr(Ipv6AddrEntry {
                dev: lo,
                addr: Ipv6Cidr::new(Ipv6Address::LOCALHOST, 128),
                scope: ROUTE_SCOPE_HOST,
            })
            .unwrap();
        router
            .add_ipv6_addr(Ipv6AddrEntry {
                dev: lo,
                addr: Ipv6Cidr::new(link_local, 64),
                scope: ROUTE_SCOPE_LINK,
            })
            .unwrap();

        let local = router
            .table
            .lookup(&IpAddress::Ipv6(Ipv6Address::LOCALHOST))
            .unwrap();
        assert_eq!(local.filter.prefix_len(), 128);
        assert_eq!(local.dev, lo);
        assert_eq!(local.table, ROUTE_TABLE_LOCAL);
        assert_eq!(local.route_type, ROUTE_TYPE_LOCAL);
        assert_eq!(local.scope, ROUTE_SCOPE_UNIVERSE);
        assert_eq!(local.protocol, ROUTE_PROTOCOL_KERNEL);
        assert!(local.is_ipv6_address_derived());

        let connected = router
            .table
            .lookup(&IpAddress::Ipv6(Ipv6Address::from_octets([
                0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,
            ])))
            .unwrap();
        assert_eq!(connected.filter.prefix_len(), 64);
        assert_eq!(connected.dev, lo);
        assert_eq!(connected.table, ROUTE_TABLE_MAIN);
        assert_eq!(connected.route_type, ROUTE_TYPE_UNICAST);
        assert_eq!(connected.scope, ROUTE_SCOPE_UNIVERSE);
        assert!(connected.is_ipv6_address_derived());

        assert_eq!(
            router.add_ipv6_addr(Ipv6AddrEntry {
                dev: lo,
                addr: Ipv6Cidr::new(Ipv6Address::UNSPECIFIED, 128),
                scope: ROUTE_SCOPE_HOST,
            }),
            Err(LinuxError::EADDRNOTAVAIL)
        );
        assert_eq!(
            router.add_ipv6_addr(Ipv6AddrEntry {
                dev: lo,
                addr: Ipv6Cidr::new(Ipv6Address::ALL_NODES_LINK_LOCAL, 128),
                scope: ROUTE_SCOPE_LINK,
            }),
            Err(LinuxError::EADDRNOTAVAIL)
        );
    }

    #[def_test]
    fn sync_netlink_rebuilds_ipv6_address_routes() {
        let mut router = Router::new();
        let lo = router.add_device(Box::new(LoopbackDevice::new()));
        router
            .add_ipv6_addr(Ipv6AddrEntry {
                dev: lo,
                addr: Ipv6Cidr::new(Ipv6Address::LOCALHOST, 128),
                scope: ROUTE_SCOPE_HOST,
            })
            .unwrap();

        router.sync_netlink(&crate::netlink::RtnetlinkState::default());

        let local = router
            .table
            .lookup(&IpAddress::Ipv6(Ipv6Address::LOCALHOST))
            .unwrap();
        assert_eq!(local.filter.prefix_len(), 128);
        assert_eq!(local.dev, lo);
        assert_eq!(local.table, ROUTE_TABLE_LOCAL);
        assert_eq!(local.route_type, ROUTE_TYPE_LOCAL);
        assert_eq!(local.scope, ROUTE_SCOPE_UNIVERSE);
        assert!(local.is_ipv6_address_derived());
        assert!(
            router
                .local_ipv6_addrs()
                .iter()
                .any(|cidr| cidr.address() == Ipv6Address::LOCALHOST)
        );
        assert_eq!(router.local_ipv6_addrs().len(), 1);
    }

    #[def_test]
    fn ipv6_multicast_source_uses_scoped_device() {
        let mut router = Router::new();
        let lo = router.add_device(Box::new(LoopbackDevice::new()));
        router
            .add_ipv6_addr(Ipv6AddrEntry {
                dev: lo,
                addr: Ipv6Cidr::new(Ipv6Address::LOCALHOST, 128),
                scope: ROUTE_SCOPE_HOST,
            })
            .unwrap();

        let dst = IpAddress::Ipv6(Ipv6Address::ALL_NODES_LINK_LOCAL);
        assert_eq!(
            router.ipv6_output_source(&dst, None, 0),
            Err(LinuxError::ENETUNREACH.into())
        );

        let (source, ifindex) = router
            .ipv6_output_source(&dst, None, lo as u32 + 1)
            .unwrap();
        assert_eq!(source, IpAddress::Ipv6(Ipv6Address::LOCALHOST));
        assert_eq!(ifindex, lo as i32 + 1);
    }
}

pub struct TxToken<'a>(&'a mut VecDeque<PacketBuf>);

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        debug_assert!(self.0.len() < SOCKET_BUFFER_SIZE);

        let mut ip_packet = vec![0u8; len];
        let result = f(&mut ip_packet);
        self.0.push_back(PacketBuf::from_ip_packet_vec(
            0,
            ip_packet,
            PacketOwner::Ipv4Stack,
        ));
        result
    }
}

pub struct RxToken(PacketBuf);

impl smoltcp::phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self
            .0
            .network_packet()
            .expect("Router RX queue only contains IP packets"))
    }
}

impl smoltcp::phy::Device for Router {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.rx_queue.is_empty() || self.data_tx_queue.len() >= DATA_TX_QUEUE_SIZE {
            None
        } else {
            Some((
                RxToken(self.rx_queue.pop_front().unwrap()),
                TxToken(&mut self.data_tx_queue),
            ))
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.data_tx_queue.len() >= DATA_TX_QUEUE_SIZE {
            None
        } else {
            Some(TxToken(&mut self.data_tx_queue))
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.effective_mtu;
        caps.max_burst_size = Some(SOCKET_BUFFER_SIZE);
        caps
    }
}
