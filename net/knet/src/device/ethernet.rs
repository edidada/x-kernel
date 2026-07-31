// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ethernet device adapter.
use alloc::{collections::VecDeque, string::String, vec::Vec};

use hashbrown::HashMap;
use kclass::{ClassDevice, prelude::*};
use kerrno::{KError, KResult, LinuxError};
use khal::time::monotonic_time;
use kirq::IrqEventSource;
use ktime_types::{MonotonicInstant, TimeSpan};

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::ETHERNET_MAX_PENDING_PACKETS,
    device::{
        ETHERNET_MAX_MTU, IPV4_MIN_MTU, LINK_FLAG_BROADCAST, LINK_FLAG_LOWER_UP,
        LINK_FLAG_MULTICAST, LINK_FLAG_RUNNING, LINK_FLAG_UP, LinkKind, LinkSnapshot,
        NeighborState, NeighborUpdate, NetDevice as NetDeviceOps,
    },
    ip::{IpAddress, Ipv4Cidr},
    packet,
    stack::ipv4::Ipv4Header,
    wire::{
        ArpIpv4Packet, ArpOperation, ETHERNET_HEADER_LEN, EtherType, EthernetFrameRef, MacAddress,
        emit_ethernet_header,
    },
};

const NET_RX_IRQ_SOURCE: IrqEventSource = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArpRequestOutcome {
    Sent,
    Failed,
}

enum ArpNeighbor {
    Incomplete {
        next_request_at: MonotonicInstant,
    },
    Reachable {
        hardware_address: MacAddress,
        expires_at: MonotonicInstant,
    },
    Permanent {
        hardware_address: MacAddress,
    },
}

impl ArpNeighbor {
    fn from_control_state(state: NeighborState) -> Self {
        match state {
            NeighborState::Incomplete => Self::Incomplete {
                next_request_at: MonotonicInstant::ORIGIN,
            },
            NeighborState::Permanent { hardware_addr } => Self::Permanent {
                hardware_address: MacAddress(hardware_addr),
            },
        }
    }

    fn learn_dynamic(&mut self, hardware_address: MacAddress, now: MonotonicInstant) {
        if !matches!(self, Self::Permanent { .. }) {
            *self = Self::Reachable {
                hardware_address,
                expires_at: now + EthernetDevice::NEIGHBOR_TTL,
            };
        }
    }

    fn record_request_outcome(&mut self, now: MonotonicInstant, outcome: ArpRequestOutcome) {
        if matches!(self, Self::Permanent { .. }) {
            return;
        }

        let retry_interval = match outcome {
            ArpRequestOutcome::Sent => EthernetDevice::ARP_REQUEST_RETRY_INTERVAL,
            ArpRequestOutcome::Failed => EthernetDevice::ARP_SEND_FAILURE_RETRY_INTERVAL,
        };
        *self = Self::Incomplete {
            next_request_at: now + retry_interval,
        };
    }

    fn next_poll_at(&self) -> Option<MonotonicInstant> {
        match self {
            Self::Incomplete { next_request_at } => Some(*next_request_at),
            Self::Reachable { expires_at, .. } => Some(*expires_at),
            Self::Permanent { .. } => None,
        }
    }

    fn hardware_address(&self, now: MonotonicInstant) -> Option<MacAddress> {
        match self {
            Self::Permanent { hardware_address } => Some(*hardware_address),
            Self::Reachable {
                hardware_address,
                expires_at,
            } if *expires_at > now => Some(*hardware_address),
            Self::Incomplete { .. } | Self::Reachable { .. } => None,
        }
    }

    fn needs_request(&self, now: MonotonicInstant) -> bool {
        match self {
            Self::Incomplete { next_request_at } => *next_request_at <= now,
            Self::Reachable { expires_at, .. } => *expires_at <= now,
            Self::Permanent { .. } => false,
        }
    }
}

/// Queued IP packet awaiting ARP resolution.
struct PendingTxPacket {
    next_hop: IpAddress,
    source_addr: crate::ip::Ipv4Address,
    next_attempt_at: TimeValue,
    packet: PacketBuf,
}

type CapturedFrame = ([u8; 6], Vec<u8>);

/// Ethernet device backed by a driver-provided NIC.
pub struct EthernetDevice {
    name: String,
    flags: u32,
    mtu: usize,
    operstate: u8,
    inner: ClassDevice<NetDeviceImpl>,
    neighbors: HashMap<IpAddress, ArpNeighbor>,
    assigned_ipv4_addrs: Vec<Ipv4Cidr>,
    local_ipv4_addrs: Vec<Ipv4Cidr>,

    pending_tx: VecDeque<PendingTxPacket>,
}

impl EthernetDevice {
    const ARP_REQUEST_RETRY_INTERVAL: TimeSpan = TimeSpan::from_secs(1);
    const ARP_SEND_FAILURE_RETRY_INTERVAL: TimeSpan = TimeSpan::from_secs(1);
    const NEIGHBOR_TTL: TimeSpan = TimeSpan::from_secs(300);
    const TX_RETRY_INTERVAL: TimeSpan = TimeSpan::from_secs(1);

    /// Create a new Ethernet device wrapper.
    pub fn new(name: String, inner: ClassDevice<NetDeviceImpl>) -> Self {
        Self {
            name,
            flags: LINK_FLAG_UP
                | LINK_FLAG_RUNNING
                | LINK_FLAG_BROADCAST
                | LINK_FLAG_MULTICAST
                | LINK_FLAG_LOWER_UP,
            mtu: crate::consts::STANDARD_MTU,
            operstate: 6,
            inner,
            neighbors: HashMap::new(),
            assigned_ipv4_addrs: Vec::new(),
            local_ipv4_addrs: Vec::new(),
            pending_tx: VecDeque::with_capacity(ETHERNET_MAX_PENDING_PACKETS),
        }
    }

    /// Spawn the RX poll task for an initialized network stack.
    pub(crate) fn spawn_rx_task() {
        let _ = ktask::spawn_with_name(
            || {
                use core::{future::poll_fn, task::Poll};

                use kpoll::PollRegistrations;

                let mut registrations = PollRegistrations::new();
                ktask::future::block_on(poll_fn(move |cx| {
                    loop {
                        crate::poll_interfaces();
                        let mut context = registrations.context(cx);
                        if kirq::register_irq_source_waker(irq, NET_RX_IRQ_SOURCE, &mut context)
                            .is_err()
                        {
                            drop(context);
                            // Sleeping without a progress registration would
                            // stall RX forever; retry under memory pressure.
                            ktask::yield_now();
                            continue;
                        }
                        drop(context);
                        // Recheck after registration to close the wakeup race.
                        crate::poll_interfaces();
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
    ) -> Result<Option<CapturedFrame>, DriverError>
    where
        F: FnOnce(&mut [u8]),
    {
        inner.recycle_tx()?;

        let src_addr = MacAddress(inner.mac().0);
        let mut tx_buf: NetBufHandle = inner.alloc_tx_buf(ETHERNET_HEADER_LEN + size)?;
        let payload = emit_ethernet_header(tx_buf.data_mut(), dst, src_addr, proto)
            .ok_or(DriverError::BadState)?;
        f(payload);
        let frame_data = packet::has_packet_handlers().then(|| tx_buf.data().to_vec());
        trace!("SEND {} bytes: {:02X?}", tx_buf.len(), tx_buf.data());
        inner.send(tx_buf)?;
        Ok(frame_data.map(|frame_data| (src_addr.bytes(), frame_data)))
    }

    fn send_to<F>(
        &mut self,
        ifindex: i32,
        dst: MacAddress,
        size: usize,
        f: F,
        proto: EtherType,
    ) -> Result<(), DriverError>
    where
        F: FnOnce(&mut [u8]),
    {
        let captured_frame = Self::send_frame(&self.inner, dst, size, f, proto)
            .inspect_err(|err| warn!("send frame failed: {:?}", err))?;
        if let Some((src_addr, frame_data)) = captured_frame {
            packet::publish_outgoing_frame(ifindex, &frame_data, src_addr);
        }
        Ok(())
    }

    fn send_ipv4_packet_to(
        &mut self,
        ifindex: i32,
        dst: MacAddress,
        packet: &PacketBuf,
    ) -> Result<(), DriverError> {
        if !self.is_link_up() {
            return Err(DriverError::BadState);
        }
        let Some(ip_packet) = packet.network_packet() else {
            return Err(DriverError::InvalidInput);
        };
        if ip_packet.len() > self.mtu {
            warn!(
                "Dropping IPv4 packet of {} bytes exceeding device MTU {}",
                ip_packet.len(),
                self.mtu
            );
            return Err(DriverError::InvalidInput);
        }
        self.send_to(
            ifindex,
            dst,
            ip_packet.len(),
            |buf| buf.copy_from_slice(ip_packet),
            EtherType::Ipv4,
        )
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

        if !dst_addr.is_broadcast() && dst_addr != MacAddress::ZERO && dst_addr != self.mac_addr() {
            return None;
        }

        match ethertype {
            EtherType::Ipv4 => {
                return Some(packet_buf);
            }
            EtherType::Arp => self.handle_arp_packet(ifindex, frame.payload(), timestamp),
            _ => {}
        }

        None
    }

    fn send_arp_request(
        &mut self,
        ifindex: i32,
        target_ip: IpAddress,
        source_addr: crate::ip::Ipv4Address,
        now: TimeValue,
    ) {
        let IpAddress::Ipv4(target_ipv4) = target_ip else {
            warn!("IPv6 address ARP is not supported: {}", target_ip);
            self.record_arp_request_outcome(target_ip, now, ArpRequestOutcome::Failed);
            return;
        };
        if source_addr.is_unspecified() {
            warn!("Cannot resolve ARP from an unspecified IPv4 source");
            self.record_arp_request_outcome(target_ip, now, ArpRequestOutcome::Failed);
            return;
        }
        debug!("Requesting ARP for {}", target_ipv4);

        let arp_packet = Self::build_arp_request(self.mac_addr(), source_addr, target_ipv4);

        let request_outcome = if self
            .send_to(
                ifindex,
                MacAddress::BROADCAST,
                ArpIpv4Packet::LEN,
                |buf| {
                    let emit_result = arp_packet.emit(buf);
                    debug_assert!(emit_result.is_some());
                },
                EtherType::Arp,
            )
            .is_ok()
        {
            ArpRequestOutcome::Sent
        } else {
            ArpRequestOutcome::Failed
        };

        self.record_arp_request_outcome(target_ip, now, request_outcome);
    }

    fn build_arp_request(
        source_hardware_addr: MacAddress,
        source_protocol_addr: crate::ip::Ipv4Address,
        target_protocol_addr: crate::ip::Ipv4Address,
    ) -> ArpIpv4Packet {
        ArpIpv4Packet {
            operation: ArpOperation::Request,
            source_hardware_addr,
            source_protocol_addr,
            target_hardware_addr: MacAddress::BROADCAST,
            target_protocol_addr,
        }
    }

    fn record_arp_request_outcome(
        &mut self,
        target_ip: IpAddress,
        now: TimeValue,
        request_outcome: ArpRequestOutcome,
    ) {
        self.neighbors
            .entry(target_ip)
            .and_modify(|neighbor| neighbor.record_request_outcome(now, request_outcome))
            .or_insert_with(|| {
                let mut neighbor = ArpNeighbor::Incomplete {
                    next_request_at: TimeValue::ZERO,
                };
                neighbor.record_request_outcome(now, request_outcome);
                neighbor
            });
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
        if !Self::contains_ipv4_address(&self.local_ipv4_addrs, target_protocol_addr) {
            return;
        }

        debug!("ARP: {} -> {}", source_protocol_addr, source_hardware_addr);
        let source_ip = IpAddress::Ipv4(source_protocol_addr);
        self.neighbors
            .entry(source_ip)
            .and_modify(|neighbor| neighbor.learn_dynamic(source_hardware_addr, now))
            .or_insert(ArpNeighbor::Reachable {
                hardware_address: source_hardware_addr,
                expires_at: now + Self::NEIGHBOR_TTL,
            });
        for pending in &mut self.pending_tx {
            if pending.next_hop == source_ip {
                pending.next_attempt_at = TimeValue::ZERO;
            }
        }

        if let ArpOperation::Request = operation {
            let response = ArpIpv4Packet {
                operation: ArpOperation::Reply,
                source_hardware_addr: self.mac_addr(),
                source_protocol_addr: target_protocol_addr,
                target_hardware_addr: source_hardware_addr,
                target_protocol_addr: source_protocol_addr,
            };

            let _ = self.send_to(
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

        self.process_pending_packets(ifindex, now);
    }

    fn process_pending_packets(&mut self, ifindex: i32, now: TimeValue) {
        if self.pending_tx.is_empty() {
            return;
        }

        enum PendingAction {
            Send(MacAddress),
            Keep,
            Refresh,
        }

        let pending_count = self.pending_tx.len();
        let mut request_targets = Vec::new();
        for _ in 0..pending_count {
            let mut pending = self.pending_tx.pop_front().unwrap();

            let action = if self.is_ipv4_broadcast(pending.next_hop) {
                if pending.next_attempt_at > now {
                    PendingAction::Keep
                } else {
                    PendingAction::Send(MacAddress::BROADCAST)
                }
            } else {
                match self.neighbors.get(&pending.next_hop) {
                    Some(neighbor) => match neighbor.hardware_address(now) {
                        Some(hardware_address) if pending.next_attempt_at <= now => {
                            PendingAction::Send(hardware_address)
                        }
                        Some(_) => PendingAction::Keep,
                        None if neighbor.needs_request(now) => PendingAction::Refresh,
                        None => PendingAction::Keep,
                    },
                    None => PendingAction::Refresh,
                }
            };

            match action {
                PendingAction::Send(hardware_address) => {
                    if let Err(err) =
                        self.send_ipv4_packet_to(ifindex, hardware_address, &pending.packet)
                        && err.should_retry()
                    {
                        pending.next_attempt_at = now.saturating_add(Self::TX_RETRY_INTERVAL);
                        self.pending_tx.push_back(pending);
                    }
                }
                PendingAction::Keep => self.pending_tx.push_back(pending),
                PendingAction::Refresh => {
                    if !request_targets
                        .iter()
                        .any(|(target, _)| *target == pending.next_hop)
                    {
                        request_targets.push((pending.next_hop, pending.source_addr));
                    }
                    self.pending_tx.push_back(pending);
                }
            }
        }

        for (target, source_addr) in request_targets {
            self.send_arp_request(ifindex, target, source_addr, now);
        }
    }

    fn is_ipv4_broadcast(&self, next_hop: IpAddress) -> bool {
        next_hop.is_broadcast()
            || self.assigned_ipv4_addrs.iter().any(|addr| {
                matches!(
                    next_hop,
                    IpAddress::Ipv4(next_hop) if addr.is_directed_broadcast(next_hop)
                )
            })
    }

    fn contains_ipv4_address(addrs: &[Ipv4Cidr], target: crate::ip::Ipv4Address) -> bool {
        addrs.iter().any(|addr| addr.address() == target)
    }

    fn earliest_pending_poll_at(
        pending_tx: &VecDeque<PendingTxPacket>,
        neighbors: &HashMap<IpAddress, ArpNeighbor>,
    ) -> Option<TimeValue> {
        pending_tx
            .iter()
            .map(|pending| {
                let neighbor_deadline = neighbors
                    .get(&pending.next_hop)
                    .and_then(ArpNeighbor::next_poll_at);
                if pending.next_attempt_at == TimeValue::ZERO {
                    neighbor_deadline.unwrap_or(TimeValue::ZERO)
                } else {
                    neighbor_deadline
                        .map(|deadline| deadline.min(pending.next_attempt_at))
                        .unwrap_or(pending.next_attempt_at)
                }
            })
            .min()
    }

    fn queue_pending_packet(&mut self, pending: PendingTxPacket) {
        if self.pending_tx.len() >= ETHERNET_MAX_PENDING_PACKETS {
            warn!("Pending packets buffer is full, dropping packet");
            return;
        }
        self.pending_tx.push_back(pending);
    }

    fn remove_pending_packets_with_source(
        pending_tx: &mut VecDeque<PendingTxPacket>,
        source_addr: crate::ip::Ipv4Address,
    ) {
        pending_tx.retain(|pending| pending.source_addr != source_addr);
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

    fn device_id(&self) -> Option<kdevice::DeviceId> {
        Some(self.inner.id())
    }

    fn poll_rx(&mut self, ifindex: i32, timestamp: MonotonicInstant) -> Option<PacketBuf> {
        if !self.is_link_up() {
            return None;
        }
        self.process_pending_packets(ifindex, timestamp);
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

    fn send_ip_packet(
        &mut self,
        ifindex: i32,
        next_hop: IpAddress,
        mut packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> bool {
        if !self.is_link_up() {
            return false;
        }
        if !matches!(next_hop, IpAddress::Ipv4(_)) {
            warn!("Dropping IPv6 packet on IPv4-only Ethernet device");
            return false;
        }

        packet.set_ifindex(ifindex);
        let Some(ip_packet) = packet.network_packet() else {
            return false;
        };
        let Ok(header) = Ipv4Header::parse_output(ip_packet) else {
            warn!("Dropping malformed IPv4 output packet");
            return false;
        };
        let source_addr = header.src_addr();
        if self.assigned_ipv4_addrs.is_empty() {
            warn!("Dropping IPv4 packet without a local IPv4 address");
            return false;
        }
        if self.is_ipv4_broadcast(next_hop) {
            if let Err(err) = self.send_ipv4_packet_to(ifindex, MacAddress::BROADCAST, &packet)
                && err.should_retry()
            {
                self.queue_pending_packet(PendingTxPacket {
                    next_hop,
                    source_addr,
                    next_attempt_at: timestamp.saturating_add(Self::TX_RETRY_INTERVAL),
                    packet,
                });
            }
            return false;
        }

        let need_request = match self.neighbors.get(&next_hop) {
            Some(neighbor) => {
                if let Some(hardware_address) = neighbor.hardware_address(timestamp) {
                    if let Err(err) = self.send_ipv4_packet_to(ifindex, hardware_address, &packet)
                        && err.should_retry()
                    {
                        self.queue_pending_packet(PendingTxPacket {
                            next_hop,
                            source_addr,
                            next_attempt_at: timestamp.saturating_add(Self::TX_RETRY_INTERVAL),
                            packet,
                        });
                    }
                    return false;
                }
                neighbor.needs_request(timestamp)
            }
            None => true,
        };
        if need_request {
            self.send_arp_request(ifindex, next_hop, source_addr, timestamp);
        }
        self.queue_pending_packet(PendingTxPacket {
            next_hop,
            source_addr,
            next_attempt_at: TimeValue::ZERO,
            packet,
        });
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
        // Ethernet registers each waiting task directly on its IRQ source;
        // the aggregated `source_waker` is only used by single-waker devices.
        if let Some(irq) = self.inner.irq() {
            kirq::register_irq_source_waker(irq, NET_RX_IRQ_SOURCE, context)?;
        }
        Ok(())
    }

    fn next_poll_at(&self) -> Option<TimeValue> {
        self.is_link_up()
            .then(|| Self::earliest_pending_poll_at(&self.pending_tx, &self.neighbors))
            .flatten()
    }

    fn set_name(&mut self, name: String) {
        self.name = name;
    }

    fn set_ipv4_addrs(&mut self, assigned_addrs: &[Ipv4Cidr], local_addrs: &[Ipv4Cidr]) {
        self.assigned_ipv4_addrs.clear();
        self.assigned_ipv4_addrs.extend_from_slice(assigned_addrs);
        self.local_ipv4_addrs.clear();
        self.local_ipv4_addrs.extend_from_slice(local_addrs);
    }

    fn remove_pending_ipv4_source(&mut self, addr: crate::ip::Ipv4Address) {
        Self::remove_pending_packets_with_source(&mut self.pending_tx, addr);
    }

    fn set_mtu(&mut self, mtu: usize) -> Result<(), LinuxError> {
        if !(IPV4_MIN_MTU..=ETHERNET_MAX_MTU).contains(&mtu) {
            return Err(LinuxError::EINVAL);
        }
        self.mtu = mtu;
        Ok(())
    }

    fn set_link_up(&mut self, is_up: bool) {
        if is_up {
            self.flags |= LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP;
            self.operstate = 6;
        } else {
            self.flags &= !(LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP);
            self.operstate = 2;
        }
    }

    fn apply_neighbor_update(&mut self, update: NeighborUpdate) -> Result<(), LinuxError> {
        let IpAddress::Ipv4(ipv4) = update.dst else {
            return Err(LinuxError::EAFNOSUPPORT);
        };
        let neighbor = ArpNeighbor::from_control_state(update.state);
        let next_hop = IpAddress::Ipv4(ipv4);
        self.neighbors.insert(next_hop, neighbor);
        for pending in &mut self.pending_tx {
            if pending.next_hop == next_hop {
                pending.next_attempt_at = MonotonicInstant::ORIGIN;
            }
        }
        }
        self.process_pending_packets(update.dev as i32 + 1, monotonic_time());
        Ok(())
    }

    fn has_neighbor(&self, dst: IpAddress) -> bool {
        self.neighbors.contains_key(&dst)
    }
}

fn map_dev_err(err: DriverError) -> KError {
    match err {
        DriverError::AlreadyExists => KError::AlreadyExists,
        DriverError::WouldBlock => KError::WouldBlock,
        DriverError::InvalidInput => KError::InvalidInput,
        DriverError::NoMemory => LinuxError::ENOBUFS.into(),
        DriverError::Io => KError::Io,
        DriverError::ResourceBusy => KError::ResourceBusy,
        DriverError::Unsupported => KError::OperationNotSupported,
        DriverError::BadState => KError::BadState,
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn incomplete_control_neighbor_requires_arp_request() {
        let mut neighbor = ArpNeighbor::from_control_state(NeighborState::Incomplete);
        let now = TimeValue::from_secs(10);

        assert!(neighbor.needs_request(now));
        neighbor.record_request_outcome(now, ArpRequestOutcome::Sent);

        assert!(!neighbor.needs_request(
            now + EthernetDevice::ARP_REQUEST_RETRY_INTERVAL - Duration::from_nanos(1)
        ));
        assert!(neighbor.needs_request(now + EthernetDevice::ARP_REQUEST_RETRY_INTERVAL));
        assert_eq!(neighbor.hardware_address(now), None);
    }

    #[def_test]
    fn expired_dynamic_neighbor_retries_arp_after_interval() {
        let expires_at = TimeValue::from_secs(10);
        let mut neighbor = ArpNeighbor::Reachable {
            hardware_address: MacAddress([1, 2, 3, 4, 5, 6]),
            expires_at,
        };

        assert!(neighbor.needs_request(expires_at));
        neighbor.record_request_outcome(expires_at, ArpRequestOutcome::Sent);

        assert!(!neighbor.needs_request(
            expires_at + EthernetDevice::ARP_REQUEST_RETRY_INTERVAL - Duration::from_nanos(1)
        ));
        assert!(neighbor.needs_request(expires_at + EthernetDevice::ARP_REQUEST_RETRY_INTERVAL));
    }

    #[def_test]
    fn failed_arp_request_retries_after_backoff() {
        let now = TimeValue::from_secs(10);
        let mut neighbor = ArpNeighbor::from_control_state(NeighborState::Incomplete);

        neighbor.record_request_outcome(now, ArpRequestOutcome::Failed);

        assert!(!neighbor.needs_request(now));
        assert!(neighbor.needs_request(now + EthernetDevice::ARP_SEND_FAILURE_RETRY_INTERVAL));
    }

    #[def_test]
    fn dynamic_arp_does_not_replace_permanent_neighbor() {
        let permanent_addr = MacAddress([1, 2, 3, 4, 5, 6]);
        let mut neighbor = ArpNeighbor::from_control_state(NeighborState::Permanent {
            hardware_addr: permanent_addr.0,
        });

        neighbor.learn_dynamic(MacAddress([6, 5, 4, 3, 2, 1]), TimeValue::ZERO);

        assert_eq!(
            neighbor.hardware_address(TimeValue::ZERO),
            Some(permanent_addr)
        );
    }

    #[def_test]
    fn pending_packet_preserves_ipv4_source() {
        let target = IpAddress::Ipv4(crate::ip::Ipv4Address::new(192, 0, 2, 1));
        let source_addr = crate::ip::Ipv4Address::new(192, 0, 2, 99);
        let pending = pending_packet(target, source_addr, TimeValue::ZERO);

        assert_eq!(pending.next_hop, target);
        assert_eq!(pending.source_addr, source_addr);
    }

    #[def_test]
    fn pending_deadline_uses_earliest_arp_or_tx_retry() {
        let target = IpAddress::Ipv4(crate::ip::Ipv4Address::new(192, 0, 2, 1));
        let source_addr = crate::ip::Ipv4Address::new(192, 0, 2, 99);
        let mut pending_tx = VecDeque::from([pending_packet(
            target,
            source_addr,
            TimeValue::from_secs(30),
        )]);
        let mut neighbors = HashMap::new();
        neighbors.insert(
            target,
            ArpNeighbor::Incomplete {
                next_request_at: TimeValue::from_secs(20),
            },
        );

        assert_eq!(
            EthernetDevice::earliest_pending_poll_at(&pending_tx, &neighbors),
            Some(TimeValue::from_secs(20))
        );

        pending_tx.front_mut().unwrap().next_attempt_at = TimeValue::from_secs(10);
        assert_eq!(
            EthernetDevice::earliest_pending_poll_at(&pending_tx, &neighbors),
            Some(TimeValue::from_secs(10))
        );
    }

    #[def_test]
    fn removing_ipv4_source_filters_only_matching_pending_packets() {
        let removed_source = crate::ip::Ipv4Address::new(192, 0, 2, 10);
        let retained_source = crate::ip::Ipv4Address::new(192, 0, 2, 20);
        let retained_target = IpAddress::Ipv4(crate::ip::Ipv4Address::new(198, 51, 100, 2));
        let mut pending_tx = VecDeque::from([
            pending_packet(
                IpAddress::Ipv4(crate::ip::Ipv4Address::new(198, 51, 100, 1)),
                removed_source,
                TimeValue::ZERO,
            ),
            pending_packet(retained_target, retained_source, TimeValue::ZERO),
            pending_packet(
                IpAddress::Ipv4(crate::ip::Ipv4Address::new(198, 51, 100, 3)),
                removed_source,
                TimeValue::from_secs(10),
            ),
        ]);

        EthernetDevice::remove_pending_packets_with_source(&mut pending_tx, removed_source);

        assert_eq!(pending_tx.len(), 1);
        let retained = pending_tx.front().unwrap();
        assert_eq!(retained.next_hop, retained_target);
        assert_eq!(retained.source_addr, retained_source);
    }

    #[def_test]
    fn permanent_and_dynamic_neighbors_expose_expected_deadlines() {
        let permanent = ArpNeighbor::Permanent {
            hardware_address: MacAddress([1, 2, 3, 4, 5, 6]),
        };
        let dynamic_expiry = TimeValue::from_secs(30);
        let dynamic = ArpNeighbor::Reachable {
            hardware_address: MacAddress([6, 5, 4, 3, 2, 1]),
            expires_at: dynamic_expiry,
        };
        let incomplete_deadline = TimeValue::from_secs(20);
        let incomplete = ArpNeighbor::Incomplete {
            next_request_at: incomplete_deadline,
        };

        assert_eq!(permanent.next_poll_at(), None);
        assert_eq!(dynamic.next_poll_at(), Some(dynamic_expiry));
        assert_eq!(incomplete.next_poll_at(), Some(incomplete_deadline));
    }

    #[def_test]
    fn multi_address_arp_request_uses_pending_packet_source() {
        let primary_addr = crate::ip::Ipv4Address::new(192, 0, 2, 2);
        let secondary_addr = crate::ip::Ipv4Address::new(192, 0, 2, 99);
        let target_addr = crate::ip::Ipv4Address::new(192, 0, 2, 1);
        let pending = pending_packet(
            IpAddress::Ipv4(target_addr),
            secondary_addr,
            TimeValue::ZERO,
        );

        let arp_request = EthernetDevice::build_arp_request(
            MacAddress([2, 0, 0, 0, 0, 2]),
            pending.source_addr,
            target_addr,
        );

        assert_ne!(arp_request.source_protocol_addr, primary_addr);
        assert_eq!(arp_request.source_protocol_addr, secondary_addr);
        assert_eq!(arp_request.target_protocol_addr, target_addr);
        assert_eq!(arp_request.operation, ArpOperation::Request);
    }

    fn pending_packet(
        next_hop: IpAddress,
        source_addr: crate::ip::Ipv4Address,
        next_attempt_at: TimeValue,
    ) -> PendingTxPacket {
        PendingTxPacket {
            next_hop,
            source_addr,
            next_attempt_at,
            packet: PacketBuf::from_ip_packet_vec(1, alloc::vec![0x45], PacketOwner::DeviceTx),
        }
    }
}
