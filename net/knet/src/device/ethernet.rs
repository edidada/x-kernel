// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ethernet device adapter.
use alloc::{string::String, vec::Vec};

use kirq::IrqEventSource;
use kclass::{ClassDevice, prelude::*};
use kerrno::{KError, KResult, LinuxError};
use khal::time::{TimeValue, monotonic_time};

use crate::{
    buf::{PacketBuf, PacketOwner},
    device::{
        LINK_FLAG_BROADCAST, LINK_FLAG_LOWER_UP, LINK_FLAG_MULTICAST, LINK_FLAG_RUNNING,
        LINK_FLAG_UP, LinkKind, LinkSnapshot, NeighborUpdate, NetDevice as NetDeviceOps,
    },
    ip::{IpAddress, Ipv4Cidr},
    link::{arp, wire, wire::MacAddress},
    neighbor::{
        NeighborAction, NeighborObservation, NeighborRequest, NeighborResolver,
        NeighborSendDecision, PendingTxPacket,
    },
    packet,
    stack::ipv4::Ipv4Header,
};

const NET_RX_IRQ_SOURCE: IrqEventSource = 0;

type CapturedFrame = ([u8; 6], Vec<u8>);

/// Ethernet device backed by a driver-provided NIC.
pub struct EthernetDevice {
    name: String,
    flags: u32,
    mtu: usize,
    operstate: u8,
    inner: ClassDevice<NetDeviceImpl>,
    neighbor_resolver: NeighborResolver,
    assigned_ipv4_addrs: Vec<Ipv4Cidr>,
    local_ipv4_addrs: Vec<Ipv4Cidr>,
}

impl EthernetDevice {
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
            neighbor_resolver: NeighborResolver::new(),
            assigned_ipv4_addrs: Vec::new(),
            local_ipv4_addrs: Vec::new(),
        }
    }

    /// Spawn the RX poll task for an initialized network stack.
    ///
    /// Register the task-owned [`PollContext`] directly with the Ethernet IRQ
    /// source. Do not route `knet-rx` through [`crate::SERVICE::register_rx_waker`]:
    /// that path also arms `timeout_poll`, and a zero-delay deadline wake would
    /// busy-spin this task and starve socket progress (including loopback TCP).
    pub(crate) fn spawn_rx_task(irq: usize) {
        let _ = ktask::spawn_with_name(
            move || {
                use alloc::boxed::Box;
                use core::{
                    future::{Future, poll_fn},
                    pin::Pin,
                    task::Poll,
                    time::Duration,
                };

                use khal::time::monotonic_time;
                use kpoll::PollRegistrations;
                use ktask::future::sleep;

                let mut registrations = PollRegistrations::new();
                // Holds the armed device-deadline sleep across `Pending` so its
                // waker stays registered until the next IRQ/timer wake.
                let mut device_timer: Option<Pin<Box<dyn Future<Output = ()> + Send>>> = None;
                ktask::future::block_on(poll_fn(move |cx| {
                    loop {
                        crate::poll_interfaces();
                        drop(device_timer.take());

                        let mut context = registrations.context(cx);
                        if kirq::register_irq_source_waker(irq, NET_RX_IRQ_SOURCE, &mut context)
                            .is_err()
                        {
                            drop(context);
                            // Sleeping without an IRQ registration would stall
                            // RX forever; yield and retry under memory pressure.
                            ktask::yield_now();
                            continue;
                        }
                        drop(context);

                        // Drive ARP / pending-TX deadlines here instead of via
                        // socket `timeout_poll` (see Service::schedule_next_poll).
                        // Never busy-spin on an already-due deadline: pending TX can
                        // remain at TimeValue::ZERO across a poll, which would
                        // otherwise starve the rest of the stack under TCG.
                        let mut now = monotonic_time();
                        let mut device_deadline = crate::SERVICE.lock().device_next_poll_at();
                        if device_deadline.is_none() {
                            // Recheck after register to close the IRQ/register race.
                            // The recheck may enqueue ARP/pending-TX work that
                            // introduces a deadline; arm `device_timer` below
                            // instead of returning Pending with no timer wake.
                            crate::poll_interfaces();
                            now = monotonic_time();
                            device_deadline = crate::SERVICE.lock().device_next_poll_at();
                        }
                        let delay = match device_deadline {
                            Some(deadline) if deadline > now => deadline.saturating_sub(now),
                            Some(_) => Duration::from_millis(1),
                            // Still no deadline after the one-shot recheck: wait
                            // for IRQ only. Do not `continue` — that would
                            // livelock when the deadline stays None.
                            None => return Poll::<()>::Pending,
                        };
                        let mut fut = Box::pin(sleep(delay));
                        if fut.as_mut().poll(cx).is_ready() {
                            continue;
                        }
                        device_timer = Some(fut);
                        // Recheck after register to close the IRQ/register race.
                        crate::poll_interfaces();
                        // Keep `device_timer` alive across this `Pending` return.
                        debug_assert!(device_timer.is_some());
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
        proto: wire::EtherType,
    ) -> DriverResult<Option<CapturedFrame>>
    where
        F: FnOnce(&mut [u8]),
    {
        inner.recycle_tx()?;

        let src_addr = MacAddress(inner.mac().0);
        let mut tx_buf: NetBufHandle = inner.alloc_tx_buf(wire::ETHERNET_HEADER_LEN + size)?;
        let payload = wire::emit_ethernet_header(tx_buf.data_mut(), dst, src_addr, proto)
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
        proto: wire::EtherType,
    ) -> DriverResult<()>
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
    ) -> DriverResult<()> {
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
            wire::EtherType::Ipv4,
        )
    }

    fn handle_rx_frame(
        &mut self,
        ifindex: i32,
        frame_data: &[u8],
        timestamp: TimeValue,
    ) -> Option<PacketBuf> {
        let Some(frame) = wire::EthernetFrameRef::new_checked(frame_data) else {
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
            wire::EtherType::Ipv4 => {
                return Some(packet_buf);
            }
            wire::EtherType::Arp => {
                self.handle_arp_frame(ifindex, dst_addr, frame.payload(), timestamp)
            }
            _ => {}
        }

        None
    }

    fn send_arp_request(&mut self, ifindex: i32, request: NeighborRequest, now: TimeValue) {
        let IpAddress::Ipv4(target_ipv4) = request.target else {
            warn!("IPv6 address ARP is not supported: {}", request.target);
            self.neighbor_resolver
                .record_request_attempt(request.target, now);
            return;
        };
        let IpAddress::Ipv4(source_addr) = request.source_addr else {
            warn!("Cannot resolve ARP from a non-IPv4 source");
            self.neighbor_resolver
                .record_request_attempt(request.target, now);
            return;
        };
        if source_addr.is_unspecified() {
            warn!("Cannot resolve ARP from an unspecified IPv4 source");
            self.neighbor_resolver
                .record_request_attempt(request.target, now);
            return;
        }
        debug!("Requesting ARP for {}", target_ipv4);

        let arp_packet = arp::build_request(self.mac_addr(), source_addr, target_ipv4);
        let _ = self.send_to(
            ifindex,
            MacAddress::BROADCAST,
            arp::ArpIpv4Packet::LEN,
            |buf| {
                let emit_result = arp_packet.emit(buf);
                debug_assert!(emit_result.is_some());
            },
            wire::EtherType::Arp,
        );

        self.neighbor_resolver
            .record_request_attempt(request.target, now);
    }

    fn send_arp_reply(
        &mut self,
        ifindex: i32,
        target_hardware_addr: MacAddress,
        response: arp::ArpIpv4Packet,
    ) {
        let _ = self.send_to(
            ifindex,
            target_hardware_addr,
            arp::ArpIpv4Packet::LEN,
            |buf| {
                let emit_result = response.emit(buf);
                debug_assert!(emit_result.is_some());
            },
            wire::EtherType::Arp,
        );
    }

    fn send_pending_packet(
        &mut self,
        ifindex: i32,
        hardware_address: MacAddress,
        pending: PendingTxPacket,
        now: TimeValue,
    ) {
        if let Err(err) = self.send_ipv4_packet_to(ifindex, hardware_address, &pending.packet)
            && err.should_retry()
        {
            self.neighbor_resolver.requeue(pending, now);
        }
    }

    fn execute_neighbor_actions(
        &mut self,
        ifindex: i32,
        actions: Vec<NeighborAction>,
        now: TimeValue,
    ) {
        for action in actions {
            match action {
                NeighborAction::Request(request) => {
                    self.send_arp_request(ifindex, request, now);
                }
                NeighborAction::Send {
                    packet,
                    hardware_address,
                } => self.send_pending_packet(ifindex, hardware_address, packet, now),
            }
        }
    }

    fn handle_arp_frame(
        &mut self,
        ifindex: i32,
        frame_destination: MacAddress,
        payload: &[u8],
        now: TimeValue,
    ) {
        let local_hardware_addr = self.mac_addr();
        let Some(event) = arp::parse_receive(
            frame_destination,
            local_hardware_addr,
            &self.local_ipv4_addrs,
            payload,
        ) else {
            debug!("Dropping malformed or irrelevant ARP packet");
            return;
        };

        debug!(
            "ARP: {} -> {}",
            event.source_protocol_addr, event.source_hardware_addr
        );
        let fallback_source_addr = self
            .assigned_ipv4_addrs
            .first()
            .map(|addr| IpAddress::Ipv4(addr.address()));
        let actions = self.neighbor_resolver.observe(
            IpAddress::Ipv4(event.source_protocol_addr),
            event.source_hardware_addr,
            now,
            if event.is_host_directed_reply {
                NeighborObservation::Reachable
            } else {
                NeighborObservation::Stale
            },
            fallback_source_addr,
        );
        if let Some(response) = event.reply(local_hardware_addr) {
            self.send_arp_reply(ifindex, event.source_hardware_addr, response);
        }
        self.execute_neighbor_actions(ifindex, actions, now);
    }

    fn process_pending_packets(&mut self, ifindex: i32, now: TimeValue) {
        let fallback_source_addr = self
            .assigned_ipv4_addrs
            .first()
            .map(|addr| IpAddress::Ipv4(addr.address()));
        let actions = self.neighbor_resolver.poll(now, fallback_source_addr);
        self.execute_neighbor_actions(ifindex, actions, now);
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

    fn poll_rx(&mut self, ifindex: i32, timestamp: TimeValue) -> Option<PacketBuf> {
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
        timestamp: TimeValue,
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
            let pending = self.neighbor_resolver.new_broadcast_packet(
                next_hop,
                IpAddress::Ipv4(source_addr),
                packet,
            );
            self.send_pending_packet(ifindex, MacAddress::BROADCAST, pending, timestamp);
            return false;
        }

        match self.neighbor_resolver.prepare_unicast(
            next_hop,
            IpAddress::Ipv4(source_addr),
            packet,
            timestamp,
        ) {
            NeighborSendDecision::Send {
                packet,
                hardware_address,
            } => self.send_pending_packet(ifindex, hardware_address, packet, timestamp),
            NeighborSendDecision::Queued {
                request: Some(request),
            } => self.send_arp_request(ifindex, request, timestamp),
            NeighborSendDecision::Queued { request: None } => {}
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
        // Ethernet registers each waiting task directly on its IRQ source;
        // the aggregated `source_waker` is only used by single-waker devices.
        if let Some(irq) = self.inner.irq() {
            kirq::register_irq_source_waker(irq, NET_RX_IRQ_SOURCE, context)?;
        }
        Ok(())
    }

    fn next_poll_at(&self) -> Option<TimeValue> {
        self.is_link_up()
            .then(|| self.neighbor_resolver.next_poll_at())
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
        self.neighbor_resolver
            .remove_pending_source(IpAddress::Ipv4(addr));
    }

    fn set_mtu(&mut self, mtu: usize) -> Result<(), LinuxError> {
        LinkKind::Ethernet.validate_mtu(mtu)?;
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
        if !matches!(update.dst, IpAddress::Ipv4(_)) {
            return Err(LinuxError::EAFNOSUPPORT);
        }
        let ifindex = update.dev as i32 + 1;
        let now = monotonic_time();
        let fallback_source_addr = self
            .assigned_ipv4_addrs
            .first()
            .map(|addr| IpAddress::Ipv4(addr.address()));
        let actions = self.neighbor_resolver.apply_control_update(
            update.dst,
            update.state,
            now,
            fallback_source_addr,
        );
        self.execute_neighbor_actions(ifindex, actions, now);
        Ok(())
    }

    fn has_neighbor(&self, dst: IpAddress) -> bool {
        self.neighbor_resolver.has_neighbor(dst)
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
