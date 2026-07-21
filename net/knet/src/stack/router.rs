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
    device::{LinkSnapshot, NeighborUpdate, NetDevice},
    ip::{IpAddress, IpCidr, Ipv4Cidr},
    udp, udp_err,
};

pub(crate) const ROUTE_TABLE_MAIN: u8 = 254;
pub(crate) const ROUTE_PROTOCOL_BOOT: u8 = 3;
pub(crate) const ROUTE_SCOPE_UNIVERSE: u8 = 0;
pub(crate) const ROUTE_SCOPE_HOST: u8 = 254;
pub(crate) const ROUTE_TYPE_UNICAST: u8 = 1;

#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub filter: IpCidr,
    pub via: Option<IpAddress>,
    pub dev: usize,
    pub src: IpAddress,
    pub mtu: usize,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    pub prefsrc: Option<IpAddress>,
}

impl Rule {
    pub fn with_route_attrs(
        filter: IpCidr,
        via: Option<IpAddress>,
        dev: usize,
        src: IpAddress,
        mtu: usize,
        attrs: RouteAttrs,
    ) -> Self {
        Self {
            filter,
            via,
            dev,
            src,
            mtu,
            table: attrs.table,
            protocol: attrs.protocol,
            scope: attrs.scope,
            route_type: attrs.route_type,
            prefsrc: attrs.prefsrc,
        }
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

#[derive(Clone, Debug, Default)]
pub(crate) struct NetworkSnapshot {
    pub links: Vec<LinkSnapshot>,
    pub addrs: Vec<Ipv4AddrEntry>,
    pub routes: Vec<Rule>,
    pub neighbors: Vec<NeighborUpdate>,
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
        let idx = self
            .rules
            .binary_search_by(|it| rule.filter.prefix_len().cmp(&it.filter.prefix_len()))
            .unwrap_or_else(|idx| idx);
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
        self.rules.retain(|rule| !same_route_rule(*rule, route));
    }

    pub fn update_device_mtu(&mut self, dev: usize, mtu: usize) {
        for rule in &mut self.rules {
            if rule.dev == dev {
                rule.mtu = mtu;
            }
        }
    }

    pub fn remove_device(&mut self, dev: usize) {
        self.rules.retain(|rule| rule.dev != dev);
        for rule in &mut self.rules {
            if rule.dev > dev {
                rule.dev -= 1;
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
        }
    }

    pub fn route_mtu(&self, dst: &IpAddress) -> Option<usize> {
        self.table.lookup(dst).map(|rule| rule.mtu)
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
        self.devices.remove(pos);
        self.table.remove_device(pos);
        self.ipv4_addrs.retain(|addr| addr.dev != pos);
        for addr in &mut self.ipv4_addrs {
            if addr.dev > pos {
                addr.dev -= 1;
            }
        }
        true
    }

    pub fn local_ipv4_addrs(&self) -> Vec<Ipv4Cidr> {
        self.ipv4_addrs.iter().map(|entry| entry.addr).collect()
    }

    pub fn network_snapshot(&self) -> NetworkSnapshot {
        NetworkSnapshot {
            links: self
                .devices
                .iter()
                .enumerate()
                .map(|(dev, device)| device.link_snapshot(dev as i32 + 1))
                .collect(),
            addrs: self.ipv4_addrs.clone(),
            routes: self.table.rules().to_vec(),
            neighbors: self
                .devices
                .iter()
                .enumerate()
                .flat_map(|(dev, device)| device.neighbor_snapshots(dev))
                .collect(),
        }
    }

    pub fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        if ifindex <= 0 {
            return None;
        }
        self.devices
            .get((ifindex - 1) as usize)
            .map(|device| device.link_snapshot(ifindex))
    }

    pub fn add_ipv4_addr(&mut self, entry: Ipv4AddrEntry) {
        if entry.dev >= self.devices.len() {
            return;
        }
        if let Some(existing) = self
            .ipv4_addrs
            .iter_mut()
            .find(|existing| existing.dev == entry.dev && existing.addr == entry.addr)
        {
            *existing = entry;
            return;
        }
        self.ipv4_addrs.push(entry);
    }

    pub fn remove_ipv4_addr(&mut self, entry: Ipv4AddrEntry) {
        self.ipv4_addrs
            .retain(|addr| addr.dev != entry.dev || addr.addr != entry.addr);
    }

    pub fn first_ipv4_addr_for_device(&self, dev: usize) -> Option<Ipv4Cidr> {
        self.ipv4_addrs
            .iter()
            .find_map(|entry| (entry.dev == dev).then_some(entry.addr))
    }

    pub fn add_route_rule(&mut self, rule: Rule) {
        if rule.dev < self.devices.len() {
            self.table.add_rule(rule);
        }
    }

    pub fn remove_exact_route_rule(&mut self, route: Rule) {
        self.table.remove_exact_rule(route);
    }

    pub fn set_device_name(&mut self, dev: usize, name: String) {
        if let Some(device) = self.devices.get_mut(dev) {
            device.set_name(name);
        }
    }

    pub fn set_device_ipv4_addr(&mut self, dev: usize, addr: Option<Ipv4Cidr>) {
        if let Some(device) = self.devices.get_mut(dev) {
            device.set_ipv4_addr(addr);
        }
    }

    pub fn set_device_mtu(&mut self, dev: usize, mtu: usize) {
        if let Some(device) = self.devices.get_mut(dev) {
            device.set_mtu(mtu);
            self.table.update_device_mtu(dev, mtu);
        }
    }

    pub fn set_device_link_up(&mut self, dev: usize, is_up: bool) {
        if let Some(device) = self.devices.get_mut(dev) {
            device.set_link_up(is_up);
        }
    }

    pub fn apply_neighbor_update(&mut self, update: NeighborUpdate) {
        if let Some(device) = self.devices.get_mut(update.dev) {
            device.apply_neighbor_update(update);
        }
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
                        let Some(rule) = self.table.lookup(&dst_addr) else {
                            warn!("No route found for destination: {}", dst_addr);
                            continue;
                        };
                        assert_eq!(rule.src, src_addr);

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
                entry.addr.address() == dst_addr || entry.addr.broadcast() == Some(dst_addr)
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
        && lhs.prefsrc == rhs.prefsrc
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
        caps.max_transmission_unit = STANDARD_MTU;
        caps.max_burst_size = Some(SOCKET_BUFFER_SIZE);
        caps
    }
}
