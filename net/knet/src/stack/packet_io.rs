// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Budgeted packet I/O and the smoltcp device adapter.

use alloc::{collections::VecDeque, vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use ktime_types::MonotonicInstant;
use smoltcp::{
    phy::{DeviceCapabilities, Medium},
    time::Instant,
    wire::Ipv6Packet,
};

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::SOCKET_BUFFER_SIZE,
    device::interfaces::{InterfaceSet, Ipv4AddrEntry},
    ip::{IpAddress, Ipv4Cidr},
    network::{
        ipv4::{self, Ipv4Error, Ipv4Header},
        router::Router,
    },
};

const CONTROL_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;
const DATA_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;

/// Owns packet queues, RX scheduling, and IPv4 output fragmentation.
///
/// The service locks this aggregate so configuration and packet dispatch share
/// one critical section, including interface removal and RX cursor adjustment.
pub(crate) struct PacketIo {
    pub(crate) routing: Router,
    pub(crate) interfaces: InterfaceSet,
    rx_queue: VecDeque<PacketBuf>,
    control_tx_queue: VecDeque<PacketBuf>,
    data_tx_queue: VecDeque<PacketBuf>,
    next_rx_device: usize,
    next_ipv4_identification: u16,
}

pub(crate) struct RxDrain {
    pub work_done: usize,
    pub has_more: bool,
}

impl PacketIo {
    pub(crate) fn new() -> Self {
        Self {
            routing: Router::new(),
            interfaces: InterfaceSet::new(),
            rx_queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            control_tx_queue: VecDeque::with_capacity(CONTROL_TX_QUEUE_SIZE),
            data_tx_queue: VecDeque::with_capacity(DATA_TX_QUEUE_SIZE),
            next_rx_device: 0,
            next_ipv4_identification: 1,
        }
    }

    /// Removes interface state and repairs the RX cursor in the same borrow.
    pub(crate) fn remove_device_by_model_id(&mut self, id: kdevice::DeviceId) -> Option<()> {
        let previous_addrs = self.interfaces.local_ipv4_addrs();
        let index = self.interfaces.remove_device_by_model_id(id)?;
        self.routing.remove_device_routes(index);
        self.refresh_routes(&previous_addrs);
        self.adjust_rx_device_cursor_after_remove(index);
        Some(())
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

    pub fn queue_ipv4_packet(&mut self, packet: Vec<u8>, oif: i32) -> KResult {
        let packets = self.fragment_ipv4_packet_for_output(packet)?;
        if !self.can_enqueue_tx_packets(packets.len()) {
            return Err(KError::WouldBlock);
        }

        self.data_tx_queue.extend(
            packets
                .into_iter()
                .map(|packet| PacketBuf::from_ip_packet_vec(oif, packet, PacketOwner::Ipv4Stack)),
        );
        Ok(())
    }

    /// Transmits an IPv4 packet through the current route without waiting for
    /// the poller TX queue.
    ///
    /// Loopback xmit raises `NetRx` and drains it on `local_bh_enable`, matching
    /// Linux `dev_queue_xmit` → `loopback_xmit` → `__netif_rx`.
    /// A full shared `NetRx` queue drops the packet and still returns `Ok`,
    /// matching `loopback_xmit` returning `NETDEV_TX_OK` after `__netif_rx`
    /// reports `NET_RX_DROP`.
    pub(crate) fn transmit_ipv4_now(
        &mut self,
        packet: Vec<u8>,
        timestamp: MonotonicInstant,
    ) -> KResult {
        let packets = self.fragment_ipv4_packet_for_output(packet)?;
        for packet in packets {
            let buf = PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack);
            // RX-ready hint for the poller, not send success. This path is
            // not the poller; a dropped loopback packet is still a successful
            // xmit, as documented above.
            let _poll_next = self.dispatch_ipv4_packet(buf, timestamp);
        }
        Ok(())
    }

    pub fn drain_rx_budgeted_into(
        &mut self,
        timestamp: MonotonicInstant,
        budget: usize,
        packets: &mut Vec<PacketBuf>,
    ) -> RxDrain {
        packets.clear();
        if budget == 0 || self.interfaces.devices().is_empty() {
            return RxDrain {
                work_done: 0,
                has_more: self.has_immediate_rx_work(),
            };
        }

        let mut work_done = 0;
        let device_count = self.interfaces.devices().len();
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
                    let dev = &mut self.interfaces.devices_mut()[dev_index];
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
                        for (dev_index, dev) in self.interfaces.devices_mut().iter_mut().enumerate()
                        {
                            poll_next |= dev.send_ip_packet(
                                dev_index as i32 + 1,
                                dst_addr,
                                src_addr,
                                packet.clone(),
                                timestamp,
                            );
                        }
                    } else {
                        let Some(rule) = self.routing.lookup(&dst_addr).copied() else {
                            warn!("No route found for destination: {}", dst_addr);
                            continue;
                        };
                        assert_eq!(rule.src, src_addr);

                        let next_hop = rule.via.unwrap_or(dst_addr);
                        let dev = &mut self.interfaces.devices_mut()[rule.dev];
                        poll_next |= dev.send_ip_packet(
                            rule.dev as i32 + 1,
                            next_hop,
                            src_addr,
                            packet,
                            timestamp,
                        );
                    }
                }
                _ => debug!("Dropping packet with invalid IP version"),
            }
        }
        (work_done, poll_next || self.has_queued_tx_packets())
    }

    /// Dispatches one IPv4 packet onto the selected device.
    ///
    /// Returns whether the send made RX ready, which [`Self::dispatch_budgeted`]
    /// uses as its `poll_next` hint. Ethernet TX and dropped packets return
    /// `false`.
    fn dispatch_ipv4_packet(&mut self, mut packet: PacketBuf, timestamp: MonotonicInstant) -> bool {
        let header = match Ipv4Header::prepare_output_packet(&mut packet) {
            Ok(header) => header,
            Err(Ipv4Error::Malformed | Ipv4Error::BadChecksum) => return false,
        };

        let src_addr = IpAddress::Ipv4(header.src_addr());
        let dst_addr = IpAddress::Ipv4(header.dst_addr());
        if header.dst_addr().is_broadcast() {
            if !(header.src_addr().is_unspecified()
                || self.interfaces.has_ipv4_address(header.src_addr()))
            {
                warn!("Dropping IPv4 broadcast packet with source {}", src_addr);
                return false;
            }

            let bound_if = packet.ifindex();
            let device_count = self.interfaces.devices().len();
            let Some((last_device, preceding_devices)) =
                self.interfaces.devices_mut().split_last_mut()
            else {
                return false;
            };

            let mut poll_next = false;
            for (dev_index, dev) in preceding_devices.iter_mut().enumerate() {
                let ifindex = dev_index as i32 + 1;
                if bound_if != 0 && bound_if != ifindex {
                    continue;
                }
                poll_next |=
                    dev.send_ip_packet(ifindex, dst_addr, src_addr, packet.clone(), timestamp);
            }
            let last_ifindex = device_count as i32;
            if bound_if == 0 || bound_if == last_ifindex {
                poll_next |=
                    last_device.send_ip_packet(last_ifindex, dst_addr, src_addr, packet, timestamp);
            }
            return poll_next;
        }

        let Some(rule) = self.routing.lookup(&dst_addr).copied() else {
            warn!("No route found for destination: {}", dst_addr);
            return false;
        };
        let bound_if = packet.ifindex();
        if bound_if != 0 && bound_if != rule.dev as i32 + 1 {
            warn!(
                "Dropping IPv4 packet bound to ifindex {bound_if} via device {}",
                rule.dev as i32 + 1
            );
            return false;
        }
        if !self.interfaces.has_ipv4_address(header.src_addr()) {
            warn!(
                "Dropping IPv4 packet with non-local source {} routed via {}",
                src_addr, rule.src
            );
            return false;
        }

        let next_hop = rule.via.unwrap_or(dst_addr);
        let dev = &mut self.interfaces.devices_mut()[rule.dev];
        dev.send_ip_packet(rule.dev as i32 + 1, next_hop, src_addr, packet, timestamp)
    }

    fn fragment_ipv4_packet_for_output(&mut self, packet: Vec<u8>) -> KResult<Vec<Vec<u8>>> {
        let header = Ipv4Header::parse_output(&packet).map_err(|_| KError::InvalidInput)?;
        let dst_addr = IpAddress::Ipv4(header.dst_addr());
        let Some(mtu) = self.routing.route_mtu(&self.interfaces, &dst_addr) else {
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

    pub fn queue_control_ipv4_packet(&mut self, packet: Vec<u8>) -> KResult {
        if self.control_tx_queue.len() >= CONTROL_TX_QUEUE_SIZE {
            return Err(KError::WouldBlock);
        }
        self.control_tx_queue.push_back(tx_packet_buf(packet));
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
        self.interfaces
            .devices()
            .iter()
            .any(|device| device.has_rx_work())
    }

    fn adjust_rx_device_cursor_after_remove(&mut self, removed_index: usize) {
        if self.interfaces.devices().is_empty() {
            self.next_rx_device = 0;
        } else if removed_index < self.next_rx_device {
            self.next_rx_device -= 1;
        } else if self.next_rx_device >= self.interfaces.devices().len() {
            self.next_rx_device = 0;
        }
    }

    // Retain the former addresses across mutation so only newly unowned sources
    // invalidate configured routes. Bootstrap rules can precede address setup.
    fn update_primary_address<T>(
        &mut self,
        dev: usize,
        update: impl FnOnce(&mut InterfaceSet) -> Result<T, LinuxError>,
    ) -> Result<T, LinuxError> {
        let previous_addr = self
            .interfaces
            .ipv4_addr_entries()
            .iter()
            .find(|entry| entry.dev == dev)
            .map(|entry| entry.addr);
        let result = update(&mut self.interfaces)?;
        self.refresh_routes(previous_addr.as_slice());
        Ok(result)
    }

    fn refresh_routes(&mut self, previous_addrs: &[Ipv4Cidr]) {
        for addr in previous_addrs {
            if !self.interfaces.has_ipv4_address(addr.address()) {
                self.routing.remove_routes_with_source(addr.address());
            }
        }
        self.routing.rebuild_ipv4_address_routes(&self.interfaces);
    }

    /// Updates an interface address and its route projection in one borrow.
    pub(crate) fn add_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        self.interfaces.add_ipv4_addr(entry)?;
        self.routing.rebuild_ipv4_address_routes(&self.interfaces);
        Ok(())
    }

    /// Changes the primary address and cleans up routes using its former source.
    pub(crate) fn set_primary_ipv4_addr(
        &mut self,
        dev: usize,
        addr: Ipv4Cidr,
    ) -> Result<(), LinuxError> {
        self.update_primary_address(dev, |interfaces| {
            interfaces.set_primary_ipv4_addr(dev, addr)
        })
    }

    /// Removes the primary address and its route projection.
    pub(crate) fn remove_primary_ipv4_addr(&mut self, dev: usize) -> Result<(), LinuxError> {
        self.update_primary_address(dev, |interfaces| interfaces.remove_primary_ipv4_addr(dev))
    }

    /// Removes an address and its route projection, reporting whether it existed.
    pub(crate) fn remove_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> bool {
        let removed = self.interfaces.remove_ipv4_addr(entry);
        if removed {
            self.refresh_routes(&[entry.addr]);
        }
        removed
    }

    /// Changes the primary prefix and refreshes connected routes.
    pub(crate) fn set_ipv4_netmask(
        &mut self,
        dev: usize,
        prefix_len: u8,
    ) -> Result<(), LinuxError> {
        self.interfaces.set_ipv4_netmask(dev, prefix_len)?;
        self.routing.rebuild_ipv4_address_routes(&self.interfaces);
        Ok(())
    }
}

fn next_device_index(dev_index: usize, device_count: usize) -> usize {
    debug_assert!(device_count > 0);
    (dev_index + 1) % device_count
}

fn tx_packet_buf(packet: Vec<u8>) -> PacketBuf {
    PacketBuf::from_ip_packet_vec(0, packet, PacketOwner::Ipv4Stack)
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, vec, vec::Vec};

    use ktime_types::MonotonicInstant;
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{device::LoopbackDevice, ip::Ipv4Address};

    fn packet_io_with_ready_loopback_packet() -> PacketIo {
        let mut packet_io = PacketIo::new();
        let loopback = packet_io
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
        let packet = PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::Ipv4Stack);
        let _ = packet_io.interfaces.devices_mut()[loopback].send_ip_packet(
            1,
            IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
            IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
            packet,
            MonotonicInstant::ORIGIN,
        );
        packet_io
    }

    #[def_test(serial)]
    fn exact_rx_budget_does_not_report_empty_device() {
        let mut packet_io = packet_io_with_ready_loopback_packet();
        let mut packets = Vec::new();

        let drain = packet_io.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 1, &mut packets);

        assert_eq!(drain.work_done, 1);
        assert!(!drain.has_more);
    }

    #[def_test(serial)]
    fn zero_rx_budget_reports_ready_device() {
        let mut packet_io = packet_io_with_ready_loopback_packet();
        let mut packets = Vec::new();

        let drain = packet_io.drain_rx_budgeted_into(MonotonicInstant::ORIGIN, 0, &mut packets);

        assert_eq!(drain.work_done, 0);
        assert!(drain.has_more);
    }

    #[def_test]
    fn control_dispatch_does_not_free_data_tx_capacity() {
        let mut packet_io = PacketIo::new();
        packet_io
            .control_tx_queue
            .push_back(tx_packet_buf(vec![0; 20]));
        let capacity_before = packet_io.available_tx_packet_slots();

        let (work_done, _) = packet_io.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);

        assert_eq!(work_done, 1);
        assert_eq!(packet_io.available_tx_packet_slots(), capacity_before);
    }

    #[def_test]
    fn data_dispatch_frees_data_tx_capacity() {
        let mut packet_io = PacketIo::new();
        packet_io
            .data_tx_queue
            .push_back(tx_packet_buf(vec![0; 20]));
        let capacity_before = packet_io.available_tx_packet_slots();

        let (work_done, _) = packet_io.dispatch_budgeted(MonotonicInstant::ORIGIN, 1);

        assert_eq!(work_done, 1);
        assert_eq!(packet_io.available_tx_packet_slots(), capacity_before + 1);
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
            .expect("PacketIo RX queue only contains IP packets"))
    }
}

impl smoltcp::phy::Device for PacketIo {
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
        caps.max_transmission_unit = self.interfaces.effective_mtu();
        caps.max_burst_size = Some(SOCKET_BUFFER_SIZE);
        caps
    }
}

#[cfg(unittest)]
mod configuration_tests {
    use alloc::boxed::Box;

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        device::LoopbackDevice,
        ip::Ipv4Address,
        router::{ROUTE_PROTOCOL_KERNEL, ROUTE_SCOPE_HOST, ROUTE_SCOPE_UNIVERSE, Rule},
    };

    #[def_test]
    fn ipv4_address_installs_local_and_connected_routes() {
        let mut router = PacketIo::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };

        router.add_ipv4_addr(entry).unwrap();

        let local = router
            .routing
            .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)))
            .unwrap();
        assert_eq!(local.filter.prefix_len(), 32);
        assert_eq!(local.dev, dev);
        assert_eq!(local.scope, ROUTE_SCOPE_HOST);
        assert_eq!(local.protocol, ROUTE_PROTOCOL_KERNEL);

        let connected = router
            .routing
            .lookup(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8)))
            .unwrap();
        assert_eq!(connected.filter.prefix_len(), 24);
        assert_eq!(connected.dev, dev);
        assert_eq!(connected.scope, ROUTE_SCOPE_HOST);
        assert_eq!(connected.protocol, ROUTE_PROTOCOL_KERNEL);
    }

    #[def_test]
    fn ipv4_broadcast_detection_covers_limited_and_directed_addresses() {
        let mut router = PacketIo::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();

        assert!(
            router
                .interfaces
                .ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::BROADCAST))
        );
        assert!(
            router
                .interfaces
                .ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 255)))
        );
        assert!(
            !router
                .interfaces
                .ipv4_dest_requires_broadcast(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 8)))
        );
    }

    #[def_test]
    fn ipv4_address_capacity_matches_smoltcp_interface_limit() {
        let mut router = PacketIo::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));

        for index in 0..smoltcp::config::IFACE_MAX_ADDR_COUNT {
            router
                .add_ipv4_addr(Ipv4AddrEntry {
                    dev,
                    addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, index as u8 + 1), 32),
                    scope: ROUTE_SCOPE_HOST,
                    broadcast: None,
                })
                .unwrap();
        }

        assert_eq!(
            router.add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 1), 32),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            }),
            Err(LinuxError::ENOBUFS)
        );
    }

    #[def_test]
    fn setting_primary_ipv4_address_keeps_secondary_addresses() {
        let mut router = PacketIo::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
        let primary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: Some(Ipv4Address::new(192, 0, 2, 111)),
        };
        let secondary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        router.add_ipv4_addr(primary).unwrap();
        router.add_ipv4_addr(secondary).unwrap();

        let replacement = Ipv4Cidr::new(Ipv4Address::new(203, 0, 113, 7), 24);
        router.set_primary_ipv4_addr(dev, replacement).unwrap();

        assert_eq!(router.interfaces.ipv4_addr_entries().len(), 2);
        assert_eq!(router.interfaces.ipv4_addr_entries()[0].addr, replacement);
        assert_eq!(
            router.interfaces.ipv4_addr_entries()[0].scope,
            ROUTE_SCOPE_UNIVERSE
        );
        assert_eq!(router.interfaces.ipv4_addr_entries()[0].broadcast, None);
        assert_eq!(router.interfaces.ipv4_addr_entries()[1], secondary);
    }

    #[def_test]
    fn removing_primary_ipv4_address_keeps_secondary_addresses() {
        let mut router = PacketIo::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
        let primary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        let secondary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };
        router.add_ipv4_addr(primary).unwrap();
        router.add_ipv4_addr(secondary).unwrap();

        router.remove_primary_ipv4_addr(dev).unwrap();

        assert_eq!(router.interfaces.ipv4_addr_entries(), &[secondary]);
    }

    #[def_test]
    fn setting_ipv4_netmask_keeps_secondary_scope_and_custom_broadcast() {
        let mut router = PacketIo::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
        let custom_broadcast = Ipv4Address::new(192, 0, 2, 111);
        let secondary = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_UNIVERSE,
            broadcast: None,
        };
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: Some(custom_broadcast),
            })
            .unwrap();
        router.add_ipv4_addr(secondary).unwrap();

        router.set_ipv4_netmask(dev, 16).unwrap();

        assert_eq!(router.interfaces.ipv4_addr_entries().len(), 2);
        assert_eq!(
            router.interfaces.ipv4_addr_entries()[0].addr.prefix_len(),
            16
        );
        assert_eq!(
            router.interfaces.ipv4_addr_entries()[0].scope,
            ROUTE_SCOPE_HOST
        );
        assert_eq!(
            router.interfaces.ipv4_addr_entries()[0].broadcast,
            Some(custom_broadcast)
        );
        assert_eq!(router.interfaces.ipv4_addr_entries()[1], secondary);
    }

    #[def_test]
    fn deleting_ipv4_address_removes_automatic_routes() {
        let mut router = PacketIo::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
        let entry = Ipv4AddrEntry {
            dev,
            addr: Ipv4Cidr::new(Ipv4Address::new(198, 51, 100, 7), 24),
            scope: ROUTE_SCOPE_HOST,
            broadcast: None,
        };

        router.add_ipv4_addr(entry).unwrap();
        assert!(
            router
                .routing
                .lookup(&IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 7)))
                .is_some()
        );

        assert!(router.remove_ipv4_addr(entry));
        assert!(
            router
                .routing
                .lookup(&IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 7)))
                .is_none()
        );
    }

    #[def_test]
    fn configured_gateway_must_be_on_device_subnet() {
        let mut router = PacketIo::new();
        let dev = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
        router
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, 2, 7), 24),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();

        let unreachable = Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(198, 51, 100, 1))),
            dev,
            IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
        );
        assert_eq!(
            router
                .routing
                .add_route_rule(&router.interfaces, unreachable),
            Err(LinuxError::ENETUNREACH)
        );

        let default = Rule::new(
            Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0).into(),
            Some(IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 1))),
            dev,
            IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 7)),
        );
        router
            .routing
            .add_route_rule(&router.interfaces, default)
            .unwrap();
        router.routing.remove_exact_route_rule(default);
        assert!(
            router
                .routing
                .route_snapshot()
                .iter()
                .all(|route| !route.is_configured())
        );
    }
}
