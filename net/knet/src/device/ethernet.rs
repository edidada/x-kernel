// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ethernet device adapter.
use alloc::{string::String, sync::Arc, vec::Vec};

use kclass::{ClassDevice, prelude::*};
use kerrno::{KError, KResult, LinuxError};
use khal::time::monotonic_time;
use kpoll::PollSet;
use ktime_types::MonotonicInstant;

use super::{
    neighbor::{NeighborCache, NeighborLearn, NeighborSnapshot, ResolveResult, SolicitKind},
    net_rx::{KnetRxScheduler, NetRxPollSource, register_net_rx_source, unregister_net_rx_source},
};
use crate::{
    buf::{PacketBuf, PacketOwner},
    device::{
        IF_OPER_DOWN, IF_OPER_UP, LINK_FLAG_BROADCAST, LINK_FLAG_LOWER_UP, LINK_FLAG_MULTICAST,
        LINK_FLAG_RUNNING, LINK_FLAG_UP, LinkKind, LinkSendSnapshot, LinkSnapshot,
        NetDevice as NetDeviceOps,
    },
    ip::{IpAddress, Ipv4Address, Ipv4Cidr, Ipv6Address, Ipv6Cidr},
    ndp, packet,
    wire::{
        ArpIpv4Packet, ArpOperation, ETHERNET_HEADER_LEN, EtherType, EthernetFrameRef, MacAddress,
        emit_ethernet_header,
    },
};

fn is_ipv4_source_usable_for_egress(
    next_hop: IpAddress,
    source_addr: Ipv4Address,
    assigned_ipv4_addrs: &[Ipv4Cidr],
) -> bool {
    next_hop.is_broadcast() || (!source_addr.is_unspecified() && !assigned_ipv4_addrs.is_empty())
}

/// Ethernet device backed by a driver-provided NIC.
pub struct EthernetDevice {
    name: String,
    flags: u32,
    mtu: usize,
    operstate: u8,
    inner: ClassDevice<NetDeviceImpl>,
    neighbors: NeighborCache,
    assigned_ipv4_addrs: Vec<Ipv4Cidr>,
    local_ipv4_addrs: Vec<Ipv4Cidr>,
    ipv6_addrs: Vec<Ipv6Cidr>,

    rx_source: Option<Arc<NetRxPollSource>>,
}

impl EthernetDevice {
    /// Create a new Ethernet device wrapper.
    pub fn new(name: String, inner: ClassDevice<NetDeviceImpl>) -> Self {
        let rx_source = if let Some(rx_source) = register_net_rx_source() {
            let scheduler: Arc<dyn NetRxScheduler> = Arc::new(KnetRxScheduler {
                source: Arc::downgrade(&rx_source),
            });
            match inner.set_rx_scheduler(Some(scheduler)) {
                Ok(()) => Some(rx_source),
                Err(DriverError::Unsupported) => {
                    warn!("Ethernet driver does not support interrupt-driven RX scheduling");
                    unregister_net_rx_source(rx_source.id());
                    None
                }
                Err(err) => {
                    warn!("failed to attach Ethernet RX scheduler: {:?}", err);
                    unregister_net_rx_source(rx_source.id());
                    None
                }
            }
        } else {
            warn!("knet: NetRx softirq unavailable; Ethernet RX uses timeout polling fallback");
            None
        };
        let mac = MacAddress(inner.mac().0);
        let link_local = Ipv6Cidr::new(Ipv6Address::link_local_from_mac(mac.bytes()), 64);
        Self {
            name,
            flags: LINK_FLAG_UP
                | LINK_FLAG_RUNNING
                | LINK_FLAG_BROADCAST
                | LINK_FLAG_MULTICAST
                | LINK_FLAG_LOWER_UP,
            mtu: crate::consts::STANDARD_MTU,
            operstate: IF_OPER_UP,
            inner,
            neighbors: NeighborCache::new(),
            assigned_ipv4_addrs: Vec::new(),
            local_ipv4_addrs: Vec::new(),
            ipv6_addrs: alloc::vec![link_local],
            rx_source,
        }
    }

    pub(crate) fn rx_poll_set(&self) -> Option<PollSet> {
        self.rx_source.as_ref().map(|source| source.waiters_clone())
    }

    /// Spawn the RX poll task for an initialized network stack.
    pub(crate) fn spawn_rx_task(rx_poll: PollSet) {
        let _ = ktask::spawn_with_name(
            move || {
                use core::{future::poll_fn, task::Poll};

                use kpoll::PollRegistrations;

                let mut registrations = PollRegistrations::new();
                ktask::future::block_on(poll_fn(move |cx| {
                    loop {
                        let mut context = registrations.context(cx);
                        if context.register(&rx_poll).is_err() {
                            drop(context);
                            // Sleeping without an RX registration would stall
                            // RX forever; yield and retry under memory pressure.
                            ktask::yield_now();
                            continue;
                        }
                        drop(context);
                        // Register before publishing the RX recheck so an IRQ
                        // arriving across this boundary either wakes this task
                        // or is covered by the bounded data-plane batch.
                        crate::poller::network_poller().publish_and_poll_rx();
                        return Poll::<()>::Pending;
                    }
                }));
            },
            "knet-rx".into(),
        );
    }

    #[inline]
    fn mac_addr(&self) -> MacAddress {
        MacAddress(self.inner.mac().0)
    }

    fn send_frame<F>(
        inner: &dyn NetDevice,
        dst: MacAddress,
        size: usize,
        f: F,
        proto: EtherType,
    ) -> Option<([u8; 6], Vec<u8>)>
    where
        F: FnOnce(&mut [u8]),
    {
        if let Err(err) = inner.recycle_tx() {
            warn!("recycle_tx failed: {:?}", err);
            return None;
        }

        let src_addr = MacAddress(inner.mac().0);
        let mut tx_buf: NetBufHandle = match inner.alloc_tx_buf(ETHERNET_HEADER_LEN + size) {
            Ok(buf) => buf,
            Err(err) => {
                warn!("alloc_tx_buf failed: {:?}", err);
                return None;
            }
        };
        let Some(payload) = emit_ethernet_header(tx_buf.data_mut(), dst, src_addr, proto) else {
            warn!("alloc_tx_buf returned a short Ethernet buffer");
            return None;
        };
        f(payload);
        let frame_data = packet::has_packet_handlers().then(|| tx_buf.data().to_vec());
        trace!("SEND {} bytes: {:02X?}", tx_buf.len(), tx_buf.data());
        if let Err(err) = inner.send(tx_buf) {
            warn!("send failed: {:?}", err);
            None
        } else {
            frame_data.map(|frame_data| (src_addr.bytes(), frame_data))
        }
    }

    fn send_to<F>(&mut self, ifindex: i32, dst: MacAddress, size: usize, f: F, proto: EtherType)
    where
        F: FnOnce(&mut [u8]),
    {
        let sent = Self::send_frame(&self.inner, dst, size, f, proto);
        if let Some((src_addr, frame_data)) = sent {
            packet::publish_outgoing_frame(ifindex, &frame_data, src_addr);
        }
    }

    fn send_ip_packet_to(
        &mut self,
        ifindex: i32,
        dst: MacAddress,
        packet: &PacketBuf,
        ethertype: EtherType,
    ) {
        if !self.is_link_up() {
            return;
        }
        let Some(ip_packet) = packet.network_packet() else {
            return;
        };
        if ip_packet.len() > self.mtu {
            warn!(
                "Dropping IPv4 packet of {} bytes exceeding device MTU {}",
                ip_packet.len(),
                self.mtu
            );
            return;
        }
        self.send_to(
            ifindex,
            dst,
            ip_packet.len(),
            |buf| buf.copy_from_slice(ip_packet),
            ethertype,
        );
    }

    fn ethertype_for_next_hop(next_hop: IpAddress) -> EtherType {
        match next_hop {
            IpAddress::Ipv4(_) => EtherType::Ipv4,
            IpAddress::Ipv6(_) => EtherType::Ipv6,
        }
    }

    fn handle_rx_frame(
        &mut self,
        ifindex: i32,
        frame_data: &[u8],
        timestamp: MonotonicInstant,
    ) -> Option<PacketBuf> {
        let Some(frame) = EthernetFrameRef::new_checked(frame_data) else {
            warn!("Dropping malformed Ethernet frame");
            return None;
        };
        let dst_addr = frame.dst_addr();
        let src_addr = frame.src_addr();
        let ethertype = frame.ethertype();

        let packet_buf = PacketBuf::from_ethernet_frame(
            ifindex,
            frame_data,
            dst_addr,
            src_addr,
            ethertype,
            self.mac_addr(),
            PacketOwner::DeviceRx,
        );
        packet::publish_link_packet(&packet_buf);

        let is_for_us = dst_addr.is_broadcast()
            || dst_addr == MacAddress::ZERO
            || dst_addr == self.mac_addr()
            || dst_addr.is_multicast();
        if !is_for_us {
            return None;
        }

        match ethertype {
            EtherType::Ipv4 | EtherType::Ipv6 => {
                return Some(packet_buf);
            }
            EtherType::Arp => self.handle_arp_packet(ifindex, frame.payload(), timestamp),
            _ => {}
        }

        None
    }

    fn send_solicit(&mut self, ifindex: i32, next_hop: IpAddress, kind: SolicitKind) {
        match next_hop {
            IpAddress::Ipv4(target_ipv4) => {
                let Some(source_addr) = self.assigned_ipv4_addrs.first().map(|cidr| cidr.address())
                else {
                    warn!("Cannot resolve ARP without a local IPv4 source");
                    return;
                };
                if source_addr.is_unspecified() {
                    warn!("Cannot resolve ARP from an unspecified IPv4 source");
                    return;
                }
                let dst_mac = match kind {
                    SolicitKind::Multicast => MacAddress::BROADCAST,
                    SolicitKind::Unicast(mac) => mac,
                };
                debug!("Requesting ARP for {}", target_ipv4);
                let arp_packet = ArpIpv4Packet {
                    operation: ArpOperation::Request,
                    source_hardware_addr: self.mac_addr(),
                    source_protocol_addr: source_addr,
                    target_hardware_addr: dst_mac,
                    target_protocol_addr: target_ipv4,
                };
                self.send_to(
                    ifindex,
                    dst_mac,
                    ArpIpv4Packet::LEN,
                    |buf| {
                        let emit_result = arp_packet.emit(buf);
                        debug_assert!(emit_result.is_some());
                    },
                    EtherType::Arp,
                );
            }
            IpAddress::Ipv6(target) => {
                let Some(src_addr) = self.ipv6_link_local() else {
                    warn!("No IPv6 source address for NDP solicitation");
                    return;
                };
                let (dst_mac, dest) = match kind {
                    SolicitKind::Multicast => (
                        MacAddress(target.solicited_node_multicast().multicast_mac()),
                        ndp::NsDestination::SolicitedNode,
                    ),
                    SolicitKind::Unicast(mac) => (mac, ndp::NsDestination::Unicast),
                };
                debug!("Requesting NDP for {}", target);
                let Some(ns_packet) =
                    ndp::build_neighbor_solicitation(src_addr, target, self.mac_addr(), dest)
                else {
                    return;
                };
                self.send_to(
                    ifindex,
                    dst_mac,
                    ns_packet.len(),
                    |buf| buf.copy_from_slice(&ns_packet),
                    EtherType::Ipv6,
                );
            }
        }
    }

    fn flush_learned(
        &mut self,
        ifindex: i32,
        next_hop: IpAddress,
        mac: MacAddress,
        packets: Vec<PacketBuf>,
    ) {
        let ethertype = Self::ethertype_for_next_hop(next_hop);
        for packet in packets {
            self.send_ip_packet_to(ifindex, mac, &packet, ethertype);
        }
    }

    fn handle_arp_packet(&mut self, ifindex: i32, payload: &[u8], now: MonotonicInstant) {
        let Some(packet) = ArpIpv4Packet::parse(payload) else {
            debug!("Dropping malformed ARP packet");
            return;
        };

        let ArpIpv4Packet {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            target_hardware_addr,
            target_protocol_addr,
        } = packet;
        let is_unicast_mac =
            target_hardware_addr != MacAddress::ZERO && !target_hardware_addr.is_broadcast();
        if is_unicast_mac && self.mac_addr() != target_hardware_addr {
            return;
        }

        if let ArpOperation::Unknown(_) = operation {
            return;
        }

        if !source_hardware_addr.is_unicast()
            || source_protocol_addr.is_broadcast()
            || source_protocol_addr.is_multicast()
            || source_protocol_addr.is_unspecified()
        {
            return;
        }
        if !self
            .local_ipv4_addrs
            .iter()
            .any(|addr| addr.address() == target_protocol_addr)
        {
            return;
        }

        debug!("ARP: {} -> {}", source_protocol_addr, source_hardware_addr);
        let addr = IpAddress::Ipv4(source_protocol_addr);
        let pending = self.neighbors.learn(
            addr,
            if matches!(operation, ArpOperation::Reply) {
                NeighborLearn::confirmed_override(source_hardware_addr)
            } else {
                NeighborLearn::unconfirmed_override(source_hardware_addr)
            },
            now,
        );
        if let Some(mac) = self.neighbors.mac(addr) {
            self.flush_learned(ifindex, addr, mac, pending);
        }

        if let ArpOperation::Request = operation {
            let response = ArpIpv4Packet {
                operation: ArpOperation::Reply,
                source_hardware_addr: self.mac_addr(),
                source_protocol_addr: target_protocol_addr,
                target_hardware_addr: source_hardware_addr,
                target_protocol_addr: source_protocol_addr,
            };

            self.send_to(
                ifindex,
                source_hardware_addr,
                ArpIpv4Packet::LEN,
                |buf| {
                    let emit_result = response.emit(buf);
                    debug_assert!(emit_result.is_some());
                },
                EtherType::Arp,
            );
        }
    }
}

impl NetDeviceOps for EthernetDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn link_kind(&self) -> LinkKind {
        LinkKind::Ethernet
    }

    fn mtu(&self) -> usize {
        self.mtu
    }

    fn is_link_up(&self) -> bool {
        self.flags & LINK_FLAG_UP != 0
    }

    fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot {
        LinkSnapshot {
            ifindex,
            name: self.name.clone(),
            flags: self.flags,
            mtu: self.mtu,
            operstate: self.operstate,
            kind: LinkKind::Ethernet,
            hardware_addr: self.mac_addr().0,
            broadcast_addr: [0xff; 6],
        }
    }

    fn link_send_snapshot(&self) -> LinkSendSnapshot {
        LinkSendSnapshot {
            is_up: self.is_link_up(),
            mtu: self.mtu,
            hardware_addr: self.mac_addr().0,
        }
    }

    fn device_id(&self) -> Option<kdevice::DeviceId> {
        Some(self.inner.id())
    }

    fn poll_rx(&mut self, ifindex: i32, timestamp: MonotonicInstant) -> Option<PacketBuf> {
        if !self.is_link_up() {
            return None;
        }
        loop {
            let rx_buf: NetBufHandle = match self.inner.recv() {
                Ok(buf) => buf,
                Err(err) => {
                    if !matches!(err, DriverError::WouldBlock) {
                        warn!("recv failed: {:?}", err);
                    }
                    return None;
                }
            };
            trace!("RECV {} bytes: {:02X?}", rx_buf.len(), rx_buf.data());

            let packet = self.handle_rx_frame(ifindex, rx_buf.data(), timestamp);
            self.inner.recycle_rx(rx_buf).unwrap();
            if packet.is_some() {
                return packet;
            }
        }
    }

    fn has_rx_work(&self) -> bool {
        self.inner.can_rx()
    }

    fn send_ip_packet(
        &mut self,
        ifindex: i32,
        next_hop: IpAddress,
        source_addr: IpAddress,
        mut packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> bool {
        if !self.is_link_up() {
            return false;
        }

        packet.set_ifindex(ifindex);
        let Some(ip_packet) = packet.network_packet() else {
            warn!("Dropping malformed output packet");
            return false;
        };
        if ip_packet.len() > self.mtu {
            warn!("Dropping packet exceeding Ethernet MTU {}", self.mtu);
            return false;
        }

        match next_hop {
            IpAddress::Ipv4(_) => {
                let IpAddress::Ipv4(source_addr) = source_addr else {
                    warn!("Dropping packet with a non-IPv4 source on Ethernet device");
                    return false;
                };
                if !is_ipv4_source_usable_for_egress(
                    next_hop,
                    source_addr,
                    &self.assigned_ipv4_addrs,
                ) {
                    warn!("Dropping IPv4 packet without a local IPv4 source");
                    return false;
                }
            }
            IpAddress::Ipv6(_) => {}
        }

        match next_hop {
            IpAddress::Ipv4(_)
                if next_hop.is_broadcast()
                    || matches!(next_hop, IpAddress::Ipv4(next_hop) if self
                        .assigned_ipv4_addrs
                        .iter()
                        .any(|addr| addr.is_directed_broadcast(next_hop))) =>
            {
                self.send_ip_packet_to(ifindex, MacAddress::BROADCAST, &packet, EtherType::Ipv4);
                return false;
            }
            IpAddress::Ipv6(addr) if addr.is_multicast() => {
                self.send_ip_packet_to(
                    ifindex,
                    MacAddress(addr.multicast_mac()),
                    &packet,
                    EtherType::Ipv6,
                );
                return false;
            }
            _ => {}
        }

        let ethertype = Self::ethertype_for_next_hop(next_hop);
        match self.neighbors.resolve(next_hop, packet, timestamp) {
            ResolveResult::Transmit { mac, packet } => {
                self.send_ip_packet_to(ifindex, mac, &packet, ethertype);
            }
            ResolveResult::Queued(solicit) => {
                if let Some(kind) = solicit {
                    self.send_solicit(ifindex, next_hop, kind);
                }
            }
            ResolveResult::Dropped => {
                warn!("Pending packets buffer is full, dropping packet");
            }
        }
        false
    }

    fn send_link_frame(&mut self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        if !self.is_link_up() {
            return Err(LinuxError::ENETDOWN.into());
        }
        let local_addr = self.mac_addr().0;
        self.inner.recycle_tx().map_err(map_dev_err)?;
        let mut tx_buf = self.inner.alloc_tx_buf(frame.len()).map_err(map_dev_err)?;
        tx_buf.data_mut().copy_from_slice(frame);
        self.inner.send(tx_buf).map_err(map_dev_err)?;
        packet::publish_outgoing_frame(ifindex, frame, local_addr);
        Ok(frame.len())
    }

    fn register_rx_waker(
        &self,
        _source_waker: &core::task::Waker,
        context: &mut kpoll::PollContext<'_>,
    ) -> Result<(), kpoll::PollRegisterError> {
        if let Some(source) = &self.rx_source {
            context.register(source.waiters())?;
        }
        Ok(())
    }

    fn set_name(&mut self, name: String) {
        self.name = name;
    }

    fn set_mtu(&mut self, mtu: usize) -> Result<(), LinuxError> {
        LinkKind::Ethernet.validate_mtu(mtu)?;
        self.mtu = mtu;
        Ok(())
    }

    fn set_link_up(&mut self, is_up: bool) {
        if is_up {
            self.flags |= LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP;
            self.operstate = IF_OPER_UP;
        } else {
            self.flags &= !(LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP);
            self.operstate = IF_OPER_DOWN;
        }
    }

    fn set_ipv4_addrs(&mut self, assigned_addrs: &[Ipv4Cidr], local_addrs: &[Ipv4Cidr]) {
        self.assigned_ipv4_addrs.clear();
        self.assigned_ipv4_addrs.extend_from_slice(assigned_addrs);
        self.local_ipv4_addrs.clear();
        self.local_ipv4_addrs.extend_from_slice(local_addrs);
    }

    fn remove_pending_ipv4_source(&mut self, addr: crate::ip::Ipv4Address) {
        self.neighbors.remove_pending_ipv4_source(addr);
    }

    fn sync_neighbors(&mut self, neighbors: &[(IpAddress, [u8; 6])]) {
        let now = monotonic_time();
        let permanent: Vec<_> = neighbors
            .iter()
            .map(|(dst_addr, hardware_addr)| (*dst_addr, MacAddress(*hardware_addr)))
            .collect();
        // Netlink-managed static neighbors stay until the control plane
        // removes them and do not run NUD. Dynamic NUD entries are kept.
        // Becoming PERMANENT over INCOMPLETE must flush pending, like learn().
        for (addr, packets) in self.neighbors.sync_permanent(&permanent, now) {
            let Some(mac) = self.neighbors.mac(addr) else {
                continue;
            };
            let ifindex = packets.first().map(|packet| packet.ifindex()).unwrap_or(0);
            self.flush_learned(ifindex, addr, mac, packets);
        }
    }

    fn set_ipv6_addrs(&mut self, addrs: &[Ipv6Cidr]) {
        if !addrs.is_empty() {
            self.ipv6_addrs = addrs.to_vec();
        }
    }

    fn neighbor_snapshots(&self, ifindex: i32) -> Vec<NeighborSnapshot> {
        self.neighbors.snapshots(ifindex)
    }

    fn learn_neighbor(&mut self, addr: IpAddress, learn: NeighborLearn) {
        let now = monotonic_time();
        let pending = self.neighbors.learn(addr, learn, now);
        let Some(mac) = self.neighbors.mac(addr) else {
            return;
        };
        let ifindex = pending.first().map(|packet| packet.ifindex()).unwrap_or(0);
        self.flush_learned(ifindex, addr, mac, pending);
    }

    fn maintain_neighbors(&mut self, ifindex: i32, now: MonotonicInstant) {
        for work in self.neighbors.run_due_timers(now) {
            self.send_solicit(ifindex, work.addr, work.solicit);
        }
    }

    fn next_neighbor_deadline(&self) -> Option<MonotonicInstant> {
        self.neighbors.next_deadline()
    }

    fn ipv6_addrs(&self) -> &[Ipv6Cidr] {
        &self.ipv6_addrs
    }

    fn hardware_addr(&self) -> Option<MacAddress> {
        Some(self.mac_addr())
    }
}

impl Drop for EthernetDevice {
    fn drop(&mut self) {
        if let Some(source) = &self.rx_source {
            if let Err(err) = self.inner.set_rx_scheduler(None) {
                warn!("failed to detach Ethernet RX scheduler: {:?}", err);
            }
            unregister_net_rx_source(source.id());
        }
    }
}

fn map_dev_err(err: DriverError) -> KError {
    match err {
        DriverError::AlreadyExists => KError::AlreadyExists,
        DriverError::WouldBlock => KError::WouldBlock,
        DriverError::InvalidInput => KError::InvalidInput,
        DriverError::NoMemory => LinuxError::ENOBUFS.into(),
        DriverError::Io => KError::Io,
        _ => KError::BadState,
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::*;

    #[def_test]
    fn limited_broadcast_accepts_unspecified_ipv4_source() {
        assert!(is_ipv4_source_usable_for_egress(
            IpAddress::Ipv4(Ipv4Address::BROADCAST),
            Ipv4Address::UNSPECIFIED,
            &[],
        ));
        assert!(!is_ipv4_source_usable_for_egress(
            IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1)),
            Ipv4Address::UNSPECIFIED,
            &[],
        ));
    }
}
