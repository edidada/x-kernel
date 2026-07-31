// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Routing table and route selection.
use alloc::{boxed::Box, collections::VecDeque, string::String, vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use ktime_types::MonotonicInstant;
use smoltcp::{
    iface::SocketSet,
    phy::{DeviceCapabilities, Medium},
    time::Instant,
    wire::{IpAddress as SmoltcpIpAddress, IpProtocol, Ipv6Packet, TcpPacket},
};

use super::{
    fragment::{Ipv4Reassembler, Ipv4ReassemblyResult},
    ipv4::{self, Icmpv4Error, Ipv4Error, Ipv4Header},
};
use crate::{
    LISTEN_TABLE,
    buf::{PacketBuf, PacketOwner},
    consts::{SOCKET_BUFFER_SIZE, STANDARD_MTU},
    device::{
        LinkConfigUpdate, LinkKind, LinkSnapshot, NeighborUpdate, NetDevice,
        validate_interface_name,
    },
    ip::{IpAddress, IpCidr, Ipv4Cidr},
    udp, udp_err,
};

pub(crate) const ROUTE_TABLE_MAIN: u8 = 254;
pub(crate) const ROUTE_PROTOCOL_KERNEL: u8 = 2;
pub(crate) const ROUTE_PROTOCOL_BOOT: u8 = 3;
pub(crate) const ROUTE_SCOPE_UNIVERSE: u8 = 0;
pub(crate) const ROUTE_SCOPE_LINK: u8 = 253;
pub(crate) const ROUTE_SCOPE_HOST: u8 = 254;
pub(crate) const ROUTE_TYPE_UNICAST: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleSource {
    DevicePrimary,
    Fixed(IpAddress),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ipv4AddressRouteKind {
    Local,
    Prefix,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleOrigin {
    Configured,
    Ipv4Address {
        owner_dev: usize,
        addr: Ipv4Cidr,
        kind: Ipv4AddressRouteKind,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub filter: IpCidr,
    pub via: Option<IpAddress>,
    pub dev: usize,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    source: RuleSource,
    origin: RuleOrigin,
}

impl Rule {
    pub fn with_route_attrs(
        filter: IpCidr,
        via: Option<IpAddress>,
        dev: usize,
        attrs: RouteAttrs,
    ) -> Self {
        Self {
            filter,
            via,
            dev,
            table: attrs.table,
            protocol: attrs.protocol,
            scope: attrs.scope,
            route_type: attrs.route_type,
            source: attrs
                .prefsrc
                .map_or(RuleSource::DevicePrimary, RuleSource::Fixed),
            origin: RuleOrigin::Configured,
        }
    }

    pub(crate) fn preferred_source(self) -> Option<IpAddress> {
        match self.source {
            RuleSource::DevicePrimary => None,
            RuleSource::Fixed(source) => Some(source),
        }
    }

    pub(crate) fn is_configured(self) -> bool {
        matches!(self.origin, RuleOrigin::Configured)
    }

    fn for_ipv4_address(
        filter: Ipv4Cidr,
        dev: usize,
        owner: Ipv4AddrEntry,
        kind: Ipv4AddressRouteKind,
        table: u8,
        scope: u8,
        route_type: u8,
    ) -> Self {
        Self {
            filter: filter.into(),
            via: None,
            dev,
            table,
            protocol: ROUTE_PROTOCOL_KERNEL,
            scope,
            route_type,
            source: RuleSource::Fixed(IpAddress::Ipv4(owner.addr.address())),
            origin: RuleOrigin::Ipv4Address {
                owner_dev: owner.dev,
                addr: owner.addr,
                kind,
            },
        }
    }

    fn lookup_rank(self) -> (u8, u8) {
        let local_rank = matches!(
            self.origin,
            RuleOrigin::Ipv4Address {
                kind: Ipv4AddressRouteKind::Local,
                ..
            }
        ) as u8;
        (self.filter.prefix_len(), local_rank)
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

#[derive(Clone, Copy, Debug)]
pub struct Ipv4AddrEntry {
    pub dev: usize,
    pub addr: Ipv4Cidr,
    pub scope: u8,
}

#[derive(Clone, Debug)]
pub(crate) struct Ipv4AddrSnapshot {
    pub entry: Ipv4AddrEntry,
    pub label: String,
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
        let rank = rule.lookup_rank();
        let idx = self
            .rules
            .partition_point(|existing| existing.lookup_rank() >= rank);
        self.rules.insert(idx, rule);
    }

    pub fn lookup(&self, dst: &IpAddress) -> Option<&Rule> {
        self.rules
            .iter()
            .find(|rule| rule.filter.contains_addr(dst))
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn remove_exact_rule(&mut self, route: Rule) {
        self.rules.retain(|rule| {
            !matches!(rule.origin, RuleOrigin::Configured) || !same_route_rule(*rule, route)
        });
    }

    fn remove_ipv4_address_routes(&mut self) {
        self.rules
            .retain(|rule| matches!(rule.origin, RuleOrigin::Configured));
    }

    fn remove_configured_routes_with_source(&mut self, source: IpAddress) {
        self.rules.retain(|rule| {
            !matches!(rule.origin, RuleOrigin::Configured)
                || rule.preferred_source() != Some(source)
        });
    }

    pub fn remove_device(&mut self, dev: usize) {
        self.rules.retain(|rule| {
            rule.dev != dev
                && !matches!(
                    rule.origin,
                    RuleOrigin::Ipv4Address { owner_dev, .. } if owner_dev == dev
                )
        });
        for rule in &mut self.rules {
            if rule.dev > dev {
                rule.dev -= 1;
            }
            if let RuleOrigin::Ipv4Address { owner_dev, .. } = &mut rule.origin
                && *owner_dev > dev
            {
                *owner_dev -= 1;
            }
        }
    }
}

pub struct Router {
    rx_queue: VecDeque<PacketBuf>,
    tx_queue: VecDeque<PacketBuf>,
    ipv4_addrs: Vec<Ipv4AddrEntry>,
    ipv4_reassembler: Ipv4Reassembler,
    next_ipv4_identification: u16,
    pub(crate) devices: Vec<Box<dyn NetDevice>>,
    pub(crate) table: RouteTable,
    effective_mtu: usize,
}
impl Router {
    pub fn new() -> Self {
        Self {
            rx_queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            tx_queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            ipv4_addrs: Vec::new(),
            ipv4_reassembler: Ipv4Reassembler::new(),
            next_ipv4_identification: 1,
            devices: Vec::new(),
            table: RouteTable::new(),
            effective_mtu: STANDARD_MTU,
        }
    }

    pub fn route_mtu(&self, dst: &IpAddress) -> Option<usize> {
        self.egress_mtu_for(dst).ok()
    }

    pub fn can_enqueue_tx_packet(&self) -> bool {
        self.tx_queue.len() < SOCKET_BUFFER_SIZE
    }

    pub fn can_enqueue_tx_packets(&self, packet_count: usize) -> bool {
        self.tx_queue
            .len()
            .checked_add(packet_count)
            .is_some_and(|len| len <= SOCKET_BUFFER_SIZE)
    }

    pub fn queue_ipv4_packet(&mut self, packet: Vec<u8>) -> KResult {
        let packets = self.fragment_ipv4_packet_for_output(packet)?;
        if !self.can_enqueue_tx_packets(packets.len()) {
            return Err(KError::WouldBlock);
        }

        self.tx_queue.extend(
            packets
                .into_iter()
                .map(|packet| PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack)),
        );
        Ok(())
    }

    pub fn add_device(&mut self, device: Box<dyn NetDevice>) -> usize {
        self.devices.push(device);
        self.sync_device_ipv4_addrs();
        self.recompute_effective_mtu();
        self.devices.len() - 1
    }

    pub fn remove_device_by_model_id(&mut self, id: kdevice::DeviceId) -> bool {
        let Some(pos) = self
            .devices
            .iter()
            .position(|device| device.device_id() == Some(id))
        else {
            return false;
        };
        let removed_sources: Vec<_> = self
            .ipv4_addrs
            .iter()
            .filter_map(|entry| (entry.dev == pos).then_some(IpAddress::Ipv4(entry.addr.address())))
            .collect();

        self.devices.remove(pos);
        self.table.remove_device(pos);
        self.ipv4_addrs.retain(|addr| addr.dev != pos);
        for addr in &mut self.ipv4_addrs {
            if addr.dev > pos {
                addr.dev -= 1;
            }
        }
        for source in removed_sources {
            if !self.has_ipv4_address(source) {
                self.table.remove_configured_routes_with_source(source);
                let IpAddress::Ipv4(source) = source else {
                    unreachable!();
                };
                for device in &mut self.devices {
                    device.remove_pending_ipv4_source(source);
                }
            }
        }
        self.rebuild_ipv4_address_routes();
        self.sync_device_ipv4_addrs();
        self.recompute_effective_mtu();
        true
    }

    pub fn local_ipv4_addrs(&self) -> Vec<Ipv4Cidr> {
        self.ipv4_addrs.iter().map(|entry| entry.addr).collect()
    }

    pub fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.devices
            .iter()
            .enumerate()
            .map(|(dev, device)| device.link_snapshot(dev as i32 + 1))
            .collect()
    }

    pub fn ipv4_addr_snapshots(&self) -> Vec<Ipv4AddrSnapshot> {
        self.ipv4_addrs
            .iter()
            .filter_map(|entry| {
                self.devices.get(entry.dev).map(|device| Ipv4AddrSnapshot {
                    entry: *entry,
                    label: String::from(device.name()),
                })
            })
            .collect()
    }

    pub fn route_snapshot(&self) -> Vec<Rule> {
        self.table.rules().to_vec()
    }

    pub fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        if ifindex <= 0 {
            return None;
        }
        self.devices
            .get((ifindex - 1) as usize)
            .map(|device| device.link_snapshot(ifindex))
    }

    pub fn has_device(&self, dev: usize) -> bool {
        dev < self.devices.len()
    }

    pub fn ipv4_addr_entries(&self) -> &[Ipv4AddrEntry] {
        &self.ipv4_addrs
    }

    pub fn route_rules(&self) -> &[Rule] {
        self.table.rules()
    }

    pub fn has_neighbor(&self, dev: usize, dst: IpAddress) -> bool {
        self.devices
            .get(dev)
            .is_some_and(|device| device.has_neighbor(dst))
    }

    pub fn add_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        if entry.dev >= self.devices.len() {
            return Err(LinuxError::ENODEV);
        }
        if entry.addr.prefix_len() > 32 || !entry.addr.address().is_unicast() {
            return Err(LinuxError::EINVAL);
        }
        if self
            .ipv4_addrs
            .iter()
            .any(|existing| existing.dev == entry.dev && existing.addr == entry.addr)
        {
            return Ok(());
        }
        if self.ipv4_addrs.len() >= smoltcp::config::IFACE_MAX_ADDR_COUNT {
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
            .retain(|addr| addr.dev != entry.dev || addr.addr != entry.addr);
        if self.ipv4_addrs.len() == old_len {
            return false;
        }

        let removed_source = IpAddress::Ipv4(entry.addr.address());
        if !self.has_ipv4_address(removed_source) {
            self.table
                .remove_configured_routes_with_source(removed_source);
            for device in &mut self.devices {
                device.remove_pending_ipv4_source(entry.addr.address());
            }
        }
        self.rebuild_ipv4_address_routes();
        self.sync_device_ipv4_addrs();
        true
    }

    pub fn source_address_for(&self, dst: &IpAddress) -> Option<IpAddress> {
        let rule = self.table.lookup(dst)?;
        if !self.devices.get(rule.dev)?.is_link_up() {
            return None;
        }
        match rule.source {
            RuleSource::Fixed(source) => Some(source),
            RuleSource::DevicePrimary => {
                let next_hop = rule.via.unwrap_or(*dst);
                self.select_ipv4_source(rule.dev, &next_hop, rule.scope)
                    .map(IpAddress::Ipv4)
            }
        }
    }

    pub fn egress_mtu_for(&self, dst: &IpAddress) -> KResult<usize> {
        if dst.is_multicast() {
            return Err(KError::OperationNotSupported);
        }
        if dst.is_broadcast() {
            return self
                .devices
                .iter()
                .filter(|device| device.is_link_up())
                .map(|device| device.mtu().min(u16::MAX as usize))
                .min()
                .ok_or(KError::from(LinuxError::ENETDOWN));
        }

        let rule = self
            .table
            .lookup(dst)
            .ok_or(KError::from(LinuxError::ENETUNREACH))?;
        let device = self
            .devices
            .get(rule.dev)
            .ok_or(KError::from(LinuxError::ENODEV))?;
        if !device.is_link_up() {
            return Err(LinuxError::ENETDOWN.into());
        }
        Ok(device.mtu().min(u16::MAX as usize))
    }

    pub fn has_ipv4_address(&self, addr: IpAddress) -> bool {
        let IpAddress::Ipv4(addr) = addr else {
            return false;
        };
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.addr.address() == addr)
    }

    pub fn is_ipv4_broadcast(&self, addr: IpAddress) -> bool {
        let IpAddress::Ipv4(addr) = addr else {
            return false;
        };
        if addr.is_broadcast() {
            return true;
        }
        if self.has_ipv4_address(IpAddress::Ipv4(addr)) {
            return false;
        }
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.addr.is_directed_broadcast(addr))
    }

    pub fn is_ipv4_bindable_broadcast(&self, addr: IpAddress) -> bool {
        let IpAddress::Ipv4(addr) = addr else {
            return false;
        };
        if addr.is_broadcast() {
            return true;
        }
        if self.has_ipv4_address(IpAddress::Ipv4(addr)) {
            return false;
        }
        self.ipv4_addrs.iter().any(|entry| {
            entry.addr.is_directed_broadcast(addr)
                && self
                    .devices
                    .get(entry.dev)
                    .is_some_and(|device| device.is_link_up())
        })
    }

    pub fn has_ipv4_subnet_for_device(&self, dev: usize, addr: IpAddress) -> bool {
        self.ipv4_addrs
            .iter()
            .any(|entry| entry.dev == dev && entry.addr.contains_addr(&addr))
    }

    pub fn is_ipv4_directed_broadcast_for_device(
        &self,
        dev: usize,
        addr: crate::ip::Ipv4Address,
    ) -> bool {
        self.ipv4_addrs
            .iter()
            .filter(|entry| entry.dev == dev)
            .any(|entry| entry.addr.is_directed_broadcast(addr))
    }

    pub fn add_route_rule(&mut self, rule: Rule) -> Result<(), LinuxError> {
        self.validate_configured_route(rule)?;
        self.table.add_rule(rule);
        Ok(())
    }

    pub fn replace_route_rule(
        &mut self,
        existing: Rule,
        replacement: Rule,
    ) -> Result<(), LinuxError> {
        self.validate_configured_route(replacement)?;
        self.table.remove_exact_rule(existing);
        self.table.add_rule(replacement);
        Ok(())
    }

    pub fn remove_exact_route_rule(&mut self, route: Rule) {
        self.table.remove_exact_rule(route);
    }

    pub fn update_device_link(
        &mut self,
        dev: usize,
        update: LinkConfigUpdate,
    ) -> Result<(), LinuxError> {
        let device = self.devices.get(dev).ok_or(LinuxError::ENODEV)?;
        if let Some(name) = update.name.as_deref() {
            validate_interface_name(name)?;
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
        if mtu.is_some() {
            self.recompute_effective_mtu();
        }
        Ok(())
    }

    fn validate_configured_route(&self, rule: Rule) -> Result<(), LinuxError> {
        if !rule.is_configured() {
            return Err(LinuxError::EINVAL);
        }
        if rule.table != ROUTE_TABLE_MAIN || rule.route_type != ROUTE_TYPE_UNICAST {
            return Err(LinuxError::EOPNOTSUPP);
        }
        if !matches!(rule.filter, IpCidr::Ipv4(_)) {
            return Err(LinuxError::EAFNOSUPPORT);
        }
        if rule.dev >= self.devices.len() {
            return Err(LinuxError::ENODEV);
        }
        if let Some(prefsrc) = rule.preferred_source()
            && !self.has_ipv4_address(prefsrc)
        {
            return Err(LinuxError::EINVAL);
        }
        if let Some(gateway) = rule.via {
            let IpAddress::Ipv4(gateway_addr) = gateway else {
                return Err(LinuxError::EAFNOSUPPORT);
            };
            if !gateway_addr.is_unicast()
                || self.is_ipv4_directed_broadcast_for_device(rule.dev, gateway_addr)
            {
                return Err(LinuxError::EINVAL);
            }
            if !self.has_ipv4_subnet_for_device(rule.dev, gateway) {
                return Err(LinuxError::ENETUNREACH);
            }
        }
        Ok(())
    }

    pub fn apply_neighbor_update(&mut self, update: NeighborUpdate) -> Result<(), LinuxError> {
        self.devices
            .get_mut(update.dev)
            .ok_or(LinuxError::ENODEV)?
            .apply_neighbor_update(update)
    }

    pub fn next_poll_at(&self) -> Option<TimeValue> {
        self.devices
            .iter()
            .filter_map(|device| device.next_poll_at())
            .min()
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

    fn select_ipv4_source(
        &self,
        dev: usize,
        next_hop: &IpAddress,
        route_scope: u8,
    ) -> Option<crate::ip::Ipv4Address> {
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

    fn rebuild_ipv4_address_routes(&mut self) {
        self.table.remove_ipv4_address_routes();
        // Local /32 host routes egress via loopback. Missing loopback must
        // only skip those entries; connected prefix routes still use the
        // address owner device and must be reinstalled after the clear above.
        let loopback_dev = self
            .devices
            .iter()
            .position(|device| device.link_kind() == LinkKind::Loopback);

        let entries = self.ipv4_addrs.clone();
        let mut routed_prefixes = Vec::new();
        for entry in entries {
            if let Some(loopback_dev) = loopback_dev {
                self.table.add_rule(Rule::for_ipv4_address(
                    Ipv4Cidr::new(entry.addr.address(), 32),
                    loopback_dev,
                    entry,
                    Ipv4AddressRouteKind::Local,
                    ROUTE_TABLE_MAIN,
                    ROUTE_SCOPE_HOST,
                    ROUTE_TYPE_UNICAST,
                ));
            }

            let network = entry.addr.network();
            if entry.addr.prefix_len() == 32
                || network.address().is_unspecified()
                || routed_prefixes.contains(&(entry.dev, network))
            {
                continue;
            }
            routed_prefixes.push((entry.dev, network));

            let scope = if self.devices[entry.dev].link_kind() == LinkKind::Loopback {
                ROUTE_SCOPE_HOST
            } else {
                ROUTE_SCOPE_LINK
            };
            self.table.add_rule(Rule::for_ipv4_address(
                network,
                entry.dev,
                entry,
                Ipv4AddressRouteKind::Prefix,
                ROUTE_TABLE_MAIN,
                scope,
                ROUTE_TYPE_UNICAST,
            ));
        }
    }

    fn recompute_effective_mtu(&mut self) {
        self.effective_mtu = self
            .devices
            .iter()
            .map(|device| device.mtu().min(u16::MAX as usize))
            .min()
            .unwrap_or(STANDARD_MTU);
    }

    pub fn poll(&mut self, timestamp: MonotonicInstant, sockets: &mut SocketSet<'_>) {
        for expired in self.ipv4_reassembler.remove_expired(timestamp) {
            self.queue_icmpv4_error(
                Icmpv4Error::FragmentReassemblyTimeout,
                expired.packet_type,
                expired.header,
                &expired.packet,
            );
        }
        for dev_index in 0..self.devices.len() {
            let ifindex = dev_index as i32 + 1;
            while self.rx_queue.len() < SOCKET_BUFFER_SIZE {
                let packet = {
                    let dev = &mut self.devices[dev_index];
                    dev.poll_rx(ifindex, timestamp)
                };
                let Some(packet) = packet else {
                    break;
                };
                let Some(ip_packet) = packet.network_packet() else {
                    continue;
                };
                match ipv4::ip_version(ip_packet) {
                    Some(4) => self.handle_ipv4_input(timestamp, packet, sockets),
                    Some(6) => self.handle_ipv6_input(packet, sockets),
                    _ => debug!("Dropping packet with invalid IP version"),
                }
            }
        }
    }

    pub fn send_link_frame(&mut self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        if ifindex <= 0 {
            return Err(KError::InvalidInput);
        }
        let dev_index = (ifindex - 1) as usize;
        let dev = self
            .devices
            .get_mut(dev_index)
            .ok_or(KError::from(LinuxError::ENODEV))?;
        dev.send_link_frame(ifindex, frame)
    }

    pub fn dispatch(&mut self, timestamp: MonotonicInstant) -> bool {
        let mut poll_next = false;
        while let Some(mut packet) = self.tx_queue.pop_front() {
            packet.set_owner(PacketOwner::DeviceTx);
            let Some(ip_packet) = packet.network_packet() else {
                continue;
            };
            match ipv4::ip_version(ip_packet) {
                Some(4) => {
                    poll_next |= self.dispatch_ipv4_packet(packet, timestamp);
                }
                Some(6) => {
                    let (src_addr, dst_addr, is_multicast) = {
                        let ip_packet =
                            Ipv6Packet::new_checked(ip_packet).expect("got invalid IPv6 packet");
                        (
                            IpAddress::Ipv6(ip_packet.src_addr().into()),
                            IpAddress::Ipv6(ip_packet.dst_addr().into()),
                            ip_packet.dst_addr().is_multicast(),
                        )
                    };
                    if is_multicast {
                        for (dev_index, dev) in self.devices.iter_mut().enumerate() {
                            poll_next |= dev.send_ip_packet(
                                dev_index as i32 + 1,
                                dst_addr,
                                packet.clone(),
                                timestamp,
                            );
                        }
                    } else {
                        let Some(rule) = self.table.lookup(&dst_addr).copied() else {
                            warn!("No route found for destination: {}", dst_addr);
                            continue;
                        };
                        let Some(route_source) = self.source_address_for(&dst_addr) else {
                            warn!("No source address found for destination: {}", dst_addr);
                            continue;
                        };
                        assert_eq!(route_source, src_addr);

                        let next_hop = rule.via.unwrap_or(dst_addr);
                        let dev = &mut self.devices[rule.dev];
                        poll_next |=
                            dev.send_ip_packet(rule.dev as i32 + 1, next_hop, packet, timestamp);
                    }
                }
                _ => debug!("Dropping packet with invalid IP version"),
            }
        }
        poll_next
    }

    fn handle_ipv4_input(
        &mut self,
        timestamp: MonotonicInstant,
        mut packet: PacketBuf,
        sockets: &mut SocketSet<'_>,
    ) {
        let header = match Ipv4Header::validate_input_packet(&mut packet) {
            Ok(header) => header,
            Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return,
        };

        if !self.is_local_ipv4_destination(header.dst_addr()) {
            return;
        }

        if header.is_fragmented() {
            match self.ipv4_reassembler.reassemble(packet, header, timestamp) {
                Ipv4ReassemblyResult::Complete(packet) => {
                    self.handle_ipv4_input(timestamp, packet, sockets)
                }
                Ipv4ReassemblyResult::Pending | Ipv4ReassemblyResult::Dropped => {}
            }
            return;
        }

        if header.protocol() == ipv4::PROTOCOL_UDP {
            let packet_type = packet.packet_type();
            let (disposition, returned_packet) = udp::handle_ipv4_packet(header, packet);
            match disposition {
                udp::InputDisposition::Accepted | udp::InputDisposition::Malformed => return,
                udp::InputDisposition::NoSocket if !header.is_broadcast_or_multicast() => {
                    let Some(packet) = returned_packet else {
                        return;
                    };
                    let Some(ip_packet) = packet.network_packet() else {
                        return;
                    };
                    self.queue_icmpv4_error(
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

        if self.should_emit_protocol_unreachable(header) {
            if let Some(ip_packet) = packet.network_packet() {
                self.queue_icmpv4_error(
                    Icmpv4Error::ProtocolUnreachable,
                    packet.packet_type(),
                    header,
                    ip_packet,
                );
            }
            return;
        }

        if let Some(ip_packet) = packet.network_packet() {
            snoop_udp_error_packet(ip_packet);
            snoop_tcp_packet(ip_packet, sockets);
        }
        self.rx_queue.push_back(packet);
    }

    fn handle_ipv6_input(&mut self, packet: PacketBuf, sockets: &mut SocketSet<'_>) {
        let Some(ip_packet) = packet.network_packet() else {
            return;
        };
        snoop_udp_error_packet(ip_packet);
        snoop_tcp_packet(ip_packet, sockets);
        self.rx_queue.push_back(packet);
    }

    fn dispatch_ipv4_packet(&mut self, mut packet: PacketBuf, timestamp: MonotonicInstant) -> bool {
        let header = match Ipv4Header::prepare_output_packet(&mut packet) {
            Ok(header) => header,
            Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return false,
        };

        let src_addr = IpAddress::Ipv4(header.src_addr());
        let dst_addr = IpAddress::Ipv4(header.dst_addr());
        if header.dst_addr().is_multicast() {
            warn!("Dropping unsupported IPv4 multicast packet to {}", dst_addr);
            return false;
        }
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
                poll_next |=
                    dev.send_ip_packet(dev_index as i32 + 1, dst_addr, packet.clone(), timestamp);
            }
            poll_next |=
                last_device.send_ip_packet(device_count as i32, dst_addr, packet, timestamp);
            return poll_next;
        }

        let Some(rule) = self.table.lookup(&dst_addr).copied() else {
            warn!("No route found for destination: {}", dst_addr);
            return false;
        };
        if !self.is_local_ipv4_source(header.src_addr()) {
            let route_source = self.source_address_for(&dst_addr);
            warn!(
                "Dropping IPv4 packet with non-local source {} routed via {:?}",
                src_addr, route_source
            );
            return false;
        }

        let next_hop = rule.via.unwrap_or(dst_addr);
        let dev = &mut self.devices[rule.dev];
        dev.send_ip_packet(rule.dev as i32 + 1, next_hop, packet, timestamp)
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

    fn queue_icmpv4_error(
        &mut self,
        error: Icmpv4Error,
        packet_type: crate::buf::PacketType,
        offending_header: Ipv4Header,
        offending_packet: &[u8],
    ) {
        if self.tx_queue.len() >= SOCKET_BUFFER_SIZE {
            warn!("TX queue is full, dropping ICMPv4 error");
            return;
        }
        let Some(packet) =
            ipv4::build_icmpv4_error_packet(error, packet_type, offending_header, offending_packet)
        else {
            return;
        };
        self.tx_queue.push_back(PacketBuf::from_ip_packet_vec(
            0,
            packet,
            PacketOwner::Ipv4Stack,
        ));
    }

    fn is_local_ipv4_destination(&self, dst_addr: crate::ip::Ipv4Address) -> bool {
        dst_addr.is_broadcast()
            || dst_addr.is_multicast()
            || self.ipv4_addrs.iter().any(|entry| {
                entry.addr.address() == dst_addr || entry.addr.is_directed_broadcast(dst_addr)
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

    fn should_emit_protocol_unreachable(&self, header: Ipv4Header) -> bool {
        !header.is_fragmented()
            && !header.is_broadcast_or_multicast()
            && !matches!(
                header.protocol(),
                ipv4::PROTOCOL_TCP | ipv4::PROTOCOL_UDP | ipv4::PROTOCOL_ICMP
            )
    }
}

fn same_route_rule(lhs: Rule, rhs: Rule) -> bool {
    lhs.filter == rhs.filter
        && lhs.via == rhs.via
        && lhs.dev == rhs.dev
        && lhs.table == rhs.table
        && lhs.protocol == rhs.protocol
        && lhs.scope == rhs.scope
        && lhs.route_type == rhs.route_type
        && lhs.preferred_source() == rhs.preferred_source()
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
            let ip_packet = Ipv6Packet::new_checked(buf).ok()?;
            // TODO: Traverse IPv6 extension headers before TCP snooping.
            // `next_header` and `payload` only refer to the base IPv6 header.
            let protocol = match ip_packet.next_header() {
                IpProtocol::Tcp => ipv4::PROTOCOL_TCP,
                IpProtocol::Udp => ipv4::PROTOCOL_UDP,
                IpProtocol::Icmp => ipv4::PROTOCOL_ICMP,
                _ => 0,
            };
            Some((
                protocol,
                SmoltcpIpAddress::Ipv6(ip_packet.src_addr()),
                SmoltcpIpAddress::Ipv6(ip_packet.dst_addr()),
                ip_packet.payload(),
            ))
        }
        _ => None,
    }
}

fn snoop_tcp_packet(buf: &[u8], sockets: &mut SocketSet<'_>) {
    let Some((protocol, src_addr, dst_addr, payload)) = parse_ip_packet(buf) else {
        return;
    };
    if protocol == ipv4::PROTOCOL_TCP {
        let Ok(tcp_packet) = TcpPacket::new_checked(payload) else {
            return;
        };
        let src_addr = (src_addr, tcp_packet.src_port()).into();
        let dst_addr = (dst_addr, tcp_packet.dst_port()).into();
        let is_first = tcp_packet.syn() && !tcp_packet.ack();
        LISTEN_TABLE.note_tcp_packet(dst_addr);
        if is_first {
            LISTEN_TABLE.incoming_tcp_packet(src_addr, dst_addr, sockets);
        }
    }
}

fn snoop_udp_error_packet(buf: &[u8]) {
    match ipv4::ip_version(buf) {
        Some(4) => udp_err::inspect_icmpv4_error(buf),
        Some(6) => {
            // UDP error queue currently supports only ICMPv4 error delivery.
        }
        _ => {}
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
        if self.rx_queue.is_empty() || self.tx_queue.len() >= SOCKET_BUFFER_SIZE {
            None
        } else {
            Some((
                RxToken(self.rx_queue.pop_front().unwrap()),
                TxToken(&mut self.tx_queue),
            ))
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.tx_queue.len() >= SOCKET_BUFFER_SIZE {
            None
        } else {
            Some(TxToken(&mut self.tx_queue))
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
