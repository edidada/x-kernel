// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Packet queues and device transport.
//!
//! `NetIo` is the queue mutex domain. Device RX still borrows `InterfaceSet`
//! while that write lock is already held; TX enqueue/dequeue does not.
use alloc::{collections::VecDeque, vec::Vec};

use kerrno::{KError, KResult};
use ktime_types::MonotonicInstant;

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::SOCKET_BUFFER_SIZE,
    device::interfaces::InterfaceSet,
};
const CONTROL_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;
pub(crate) const DATA_TX_QUEUE_SIZE: usize = SOCKET_BUFFER_SIZE;
pub(crate) struct NetIo {
    pub(crate) rx_queue: VecDeque<PacketBuf>,
    control_tx_queue: VecDeque<PacketBuf>,
    pub(crate) data_tx_queue: VecDeque<PacketBuf>,
    next_rx_device: usize,
}
pub(crate) struct RxDrain {
    pub work_done: usize,
    pub has_more: bool,
}
impl NetIo {
    pub(crate) fn new() -> Self {
        Self {
            rx_queue: VecDeque::with_capacity(SOCKET_BUFFER_SIZE),
            control_tx_queue: VecDeque::with_capacity(CONTROL_TX_QUEUE_SIZE),
            data_tx_queue: VecDeque::with_capacity(DATA_TX_QUEUE_SIZE),
            next_rx_device: 0,
        }
    }

    /// Appends a complete output batch after capacity preflight under the same transaction.
    pub(crate) fn enqueue_tx(&mut self, packets: impl ExactSizeIterator<Item = PacketBuf>) {
        debug_assert!(self.can_enqueue_tx_packets(packets.len()));
        self.data_tx_queue.extend(packets);
    }

    pub fn can_enqueue_tx_packet(&self) -> bool {
        self.data_tx_queue.len() < DATA_TX_QUEUE_SIZE
    }

    pub(crate) fn available_data_tx_slots(&self) -> usize {
        DATA_TX_QUEUE_SIZE.saturating_sub(self.data_tx_queue.len())
    }

    pub fn can_enqueue_tx_packets(&self, packet_count: usize) -> bool {
        self.data_tx_queue
            .len()
            .checked_add(packet_count)
            .is_some_and(|len| len <= DATA_TX_QUEUE_SIZE)
    }

    pub fn drain_device_rx_budgeted(
        &mut self,
        interfaces: &mut InterfaceSet,
        timestamp: MonotonicInstant,
        budget: usize,
        packets: &mut Vec<PacketBuf>,
    ) -> RxDrain {
        packets.clear();
        if budget == 0 || interfaces.devices().is_empty() {
            return RxDrain {
                work_done: 0,
                has_more: interfaces.has_rx_work(),
            };
        }

        let mut work_done = 0;
        let device_count = interfaces.devices().len();
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
                        has_more: interfaces.has_rx_work(),
                    };
                }

                let packet = {
                    let dev = &mut interfaces.devices_mut()[dev_index];
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

    pub fn enqueue_stack_ingress(&mut self, packets: &mut Vec<PacketBuf>) {
        debug_assert!(packets.len() <= self.stack_ingress_capacity());
        self.rx_queue.extend(packets.drain(..));
    }

    pub fn stack_ingress_capacity(&self) -> usize {
        SOCKET_BUFFER_SIZE.saturating_sub(self.rx_queue.len())
    }

    pub fn has_stack_ingress(&self) -> bool {
        !self.rx_queue.is_empty()
    }

    pub fn enqueue_control_tx(&mut self, packet: Vec<u8>) -> KResult {
        if self.control_tx_queue.len() >= CONTROL_TX_QUEUE_SIZE {
            return Err(KError::WouldBlock);
        }
        self.control_tx_queue.push_back(tx_packet_buf(packet));
        Ok(())
    }

    pub(crate) fn dequeue_tx(&mut self) -> Option<PacketBuf> {
        self.control_tx_queue
            .pop_front()
            .or_else(|| self.data_tx_queue.pop_front())
    }

    pub(crate) fn has_pending_tx(&self) -> bool {
        !self.control_tx_queue.is_empty() || !self.data_tx_queue.is_empty()
    }

    pub(crate) fn on_device_removed(&mut self, interfaces: &InterfaceSet, removed_index: usize) {
        if interfaces.devices().is_empty() {
            self.next_rx_device = 0;
        } else if removed_index < self.next_rx_device {
            self.next_rx_device -= 1;
        } else if self.next_rx_device >= interfaces.devices().len() {
            self.next_rx_device = 0;
        }
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
    use alloc::{boxed::Box, vec};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        device::LoopbackDevice,
        ip::{IpAddress, Ipv4Address},
        network::state::NetworkState,
    };
    fn network_with_ready_loopback_packet() -> (NetworkState, NetIo) {
        let packet_io = NetworkState::new();
        let loopback = packet_io.add_device(Box::new(LoopbackDevice::new()));
        let packet = PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::Ipv4Stack);
        packet_io.with_interfaces_mut(|interfaces| {
            interfaces.devices_mut()[loopback].send_ip_packet(
                loopback as i32 + 1,
                IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
                IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
                packet,
                MonotonicInstant::ORIGIN,
            )
        });
        (packet_io, NetIo::new())
    }
    #[def_test(serial)]
    fn exact_rx_budget_does_not_report_empty_device() {
        let (state, mut io) = network_with_ready_loopback_packet();
        let mut packets = Vec::new();

        let drain = state.with_interfaces_mut(|interfaces| {
            io.drain_device_rx_budgeted(interfaces, MonotonicInstant::ORIGIN, 1, &mut packets)
        });

        assert_eq!(drain.work_done, 1);
        assert!(!drain.has_more);
    }
    #[def_test(serial)]
    fn zero_rx_budget_reports_ready_device() {
        let (state, mut io) = network_with_ready_loopback_packet();
        let mut packets = Vec::new();

        let drain = state.with_interfaces_mut(|interfaces| {
            io.drain_device_rx_budgeted(interfaces, MonotonicInstant::ORIGIN, 0, &mut packets)
        });

        assert_eq!(drain.work_done, 0);
        assert!(drain.has_more);
    }

    #[def_test]
    fn device_removal_repairs_cursor_and_route_indices() {
        use crate::{
            device::{interfaces::Ipv4AddrEntry, test_device::TestDevice},
            ip::Ipv4Cidr,
            router::ROUTE_SCOPE_UNIVERSE,
            service::Service,
        };
        let state = NetworkState::new();
        for index in 0..3 {
            let dev = state.add_device(Box::new(TestDevice::ethernet(index + 1, 1500)));
            state
                .add_ipv4_addr(Ipv4AddrEntry {
                    dev,
                    addr: Ipv4Cidr::new(Ipv4Address::new(192, 0, index as u8, 1), 24),
                    scope: ROUTE_SCOPE_UNIVERSE,
                    broadcast: None,
                })
                .unwrap();
        }
        let service = Service::new(state);
        service.with_io(|io| io.next_rx_device = 2);
        let config = crate::control::NetworkConfig::lock(&service);
        config.remove_device(kdevice::DeviceId::new(2)).unwrap();
        let cursor = service.with_io(|io| io.next_rx_device);
        let route_dev = service
            .network()
            .lookup_route(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 2)))
            .unwrap()
            .dev;
        let removed_route = service
            .network()
            .lookup_route(&IpAddress::Ipv4(Ipv4Address::new(192, 0, 1, 2)))
            .is_some();
        assert_eq!(cursor, 1);
        assert_eq!(route_dev, 1);
        assert!(!removed_route);
    }
}
