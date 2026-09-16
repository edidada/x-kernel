// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Packet queues and device transport.
//!
//! `NetIo` is the queue mutex domain. Device RX borrows [`DeviceHandle`]s
//! under the inventory read lock and polls a bounded batch per device mutex.
//! TX enqueue/dequeue does not borrow the inventory.
use alloc::{collections::VecDeque, vec::Vec};

use kerrno::{KError, KResult};
use ktime_types::MonotonicInstant;

use crate::{
    buf::{PacketBuf, PacketOwner},
    device::DeviceHandle,
};

const STACK_RX_QUEUE_CAPACITY: usize = 64;
const DATA_TX_QUEUE_CAPACITY: usize = 64;
const CONTROL_TX_QUEUE_CAPACITY: usize = 64;

struct BoundedQueue<T> {
    queue: VecDeque<T>,
    capacity: usize,
}

impl<T> BoundedQueue<T> {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn available(&self) -> usize {
        self.capacity.saturating_sub(self.queue.len())
    }

    fn is_full(&self) -> bool {
        self.queue.len() >= self.capacity
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    fn can_push(&self, count: usize) -> bool {
        count <= self.available()
    }

    fn try_push(&mut self, value: T) -> Result<(), T> {
        if self.is_full() {
            return Err(value);
        }

        self.queue.push_back(value);
        Ok(())
    }

    fn pop_front(&mut self) -> Option<T> {
        self.queue.pop_front()
    }

    fn try_extend<I>(&mut self, items: I) -> Result<(), I>
    where
        I: ExactSizeIterator<Item = T>,
    {
        if !self.can_push(items.len()) {
            return Err(items);
        }

        self.queue.extend(items);
        Ok(())
    }
}

pub(crate) struct NetIo {
    rx_queue: BoundedQueue<PacketBuf>,
    control_tx_queue: BoundedQueue<PacketBuf>,
    data_tx_queue: BoundedQueue<PacketBuf>,
    next_rx_device: usize,
}

pub(crate) struct RxDrain {
    pub work_done: usize,
    pub has_more: bool,
    pub has_lock_contention: bool,
}

impl NetIo {
    pub(crate) fn new() -> Self {
        Self {
            rx_queue: BoundedQueue::with_capacity(STACK_RX_QUEUE_CAPACITY),
            control_tx_queue: BoundedQueue::with_capacity(CONTROL_TX_QUEUE_CAPACITY),
            data_tx_queue: BoundedQueue::with_capacity(DATA_TX_QUEUE_CAPACITY),
            next_rx_device: 0,
        }
    }

    pub fn available_stack_ingress_slots(&self) -> usize {
        self.rx_queue.available()
    }

    /// Returns whether smoltcp may emit one stack-originated datagram.
    pub fn can_transmit_from_stack(&self) -> bool {
        !self.data_tx_queue.is_full()
    }

    /// Returns whether IPv4 egress can admit `packet_count` data-TX packets.
    pub fn can_admit_ipv4_egress(&self, packet_count: usize) -> bool {
        self.data_tx_queue.can_push(packet_count)
    }

    /// Appends a complete output batch after capacity preflight under the same transaction.
    pub(crate) fn enqueue_tx(&mut self, packets: impl ExactSizeIterator<Item = PacketBuf>) {
        debug_assert!(self.data_tx_queue.can_push(packets.len()));
        if self.data_tx_queue.try_extend(packets).is_err() {
            panic!("enqueue_tx requires prior IPv4 egress capacity preflight");
        }
    }

    pub fn can_receive_from_stack(&self) -> bool {
        !self.rx_queue.is_empty() && self.can_transmit_from_stack()
    }

    pub(crate) fn pop_stack_ingress(&mut self) -> Option<PacketBuf> {
        self.rx_queue.pop_front()
    }

    pub(crate) fn enqueue_stack_tx(&mut self, ip_packet: Vec<u8>) {
        self.data_tx_queue
            .try_push(tx_packet_buf(ip_packet))
            .expect("stack TX enqueue requires a free data slot");
    }

    pub(crate) fn pull_tx_packets(
        &mut self,
        budget: usize,
        packets: &mut VecDeque<PacketBuf>,
    ) -> bool {
        let capacity_before = self.data_tx_queue.available();
        let base_len = packets.len();
        while packets.len() - base_len < budget {
            let Some(packet) = self.dequeue_tx() else {
                break;
            };
            packets.push_back(packet);
        }
        self.data_tx_queue.available() > capacity_before
    }

    pub fn drain_device_rx_budgeted(
        &mut self,
        devices: &[DeviceHandle],
        timestamp: MonotonicInstant,
        budget: usize,
        packets: &mut Vec<PacketBuf>,
    ) -> RxDrain {
        packets.clear();

        let has_rx_work = || devices.iter().any(|device| device.has_rx_work());

        if budget == 0 || devices.is_empty() {
            return RxDrain {
                work_done: 0,
                has_more: has_rx_work(),
                has_lock_contention: false,
            };
        }

        let device_count = devices.len();
        let mut work_done = 0;
        let mut has_lock_contention = false;

        self.next_rx_device %= device_count;

        for _ in 0..device_count {
            let dev_index = self.next_rx_device;
            let ifindex = dev_index as i32 + 1;

            match devices[dev_index].try_lock() {
                Some(mut device) => loop {
                    if work_done >= budget {
                        self.next_rx_device = next_device_index(dev_index, device_count);

                        drop(device);

                        return RxDrain {
                            work_done,
                            has_more: has_lock_contention || has_rx_work(),
                            has_lock_contention,
                        };
                    }

                    match device.poll_rx(ifindex, timestamp) {
                        Some(packet) => {
                            work_done += 1;
                            packets.push(packet);
                        }
                        None => break,
                    }
                },

                None => {
                    has_lock_contention = true;
                }
            }

            self.next_rx_device = next_device_index(dev_index, device_count);
        }

        RxDrain {
            work_done,
            has_more: has_lock_contention,
            has_lock_contention,
        }
    }

    pub fn enqueue_stack_ingress(&mut self, packets: &mut Vec<PacketBuf>) {
        debug_assert!(self.rx_queue.can_push(packets.len()));
        if self.rx_queue.try_extend(packets.drain(..)).is_err() {
            panic!("stack ingress batch exceeds available RX slots");
        }
    }

    pub fn has_stack_ingress(&self) -> bool {
        !self.rx_queue.is_empty()
    }

    pub fn enqueue_control_tx(&mut self, packet: Vec<u8>) -> KResult {
        self.control_tx_queue
            .try_push(tx_packet_buf(packet))
            .map_err(|_| KError::WouldBlock)
    }

    pub(crate) fn dequeue_tx(&mut self) -> Option<PacketBuf> {
        self.control_tx_queue
            .pop_front()
            .or_else(|| self.data_tx_queue.pop_front())
    }

    pub(crate) fn has_pending_tx(&self) -> bool {
        !self.control_tx_queue.is_empty() || !self.data_tx_queue.is_empty()
    }

    pub(crate) fn on_device_removed(&mut self, device_count: usize, removed_index: usize) {
        if device_count == 0 {
            self.next_rx_device = 0;
        } else if removed_index < self.next_rx_device {
            self.next_rx_device -= 1;
        } else if self.next_rx_device >= device_count {
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
    use alloc::{boxed::Box, vec, vec::Vec};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        device::LoopbackDevice,
        ip::{IpAddress, Ipv4Address},
        network::state::NetworkState,
    };

    #[def_test]
    fn bounded_queue_rejects_overflow() {
        let mut queue = BoundedQueue::with_capacity(1);
        queue.try_push(1).unwrap();
        assert!(queue.is_full());
        assert_eq!(queue.available(), 0);
        assert_eq!(queue.try_push(2), Err(2));
    }

    #[def_test]
    fn bounded_queue_try_extend_returns_items_when_full() {
        let mut queue = BoundedQueue::with_capacity(2);
        assert!(queue.try_extend([1, 2].into_iter()).is_ok());
        assert!(queue.is_full());
        let rejected = queue.try_extend([3].into_iter()).unwrap_err();
        assert_eq!(rejected.collect::<Vec<_>>(), vec![3]);
        assert_eq!(queue.available(), 0);
        assert!(queue.try_extend(core::iter::empty()).is_ok());
    }

    fn network_with_ready_loopback_packet() -> (NetworkState, NetIo) {
        let packet_io = NetworkState::new();
        let loopback = packet_io.add_device(Box::new(LoopbackDevice::new()));
        let packet = PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::Ipv4Stack);
        packet_io.with_interfaces(|interfaces| {
            interfaces.devices()[loopback].send_ip_packet(
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

        let drain = state.with_interfaces(|interfaces| {
            io.drain_device_rx_budgeted(
                interfaces.devices(),
                MonotonicInstant::ORIGIN,
                1,
                &mut packets,
            )
        });

        assert_eq!(drain.work_done, 1);
        assert!(!drain.has_more);
    }
    #[def_test(serial)]
    fn zero_rx_budget_reports_ready_device() {
        let (state, mut io) = network_with_ready_loopback_packet();
        let mut packets = Vec::new();

        let drain = state.with_interfaces(|interfaces| {
            io.drain_device_rx_budgeted(
                interfaces.devices(),
                MonotonicInstant::ORIGIN,
                0,
                &mut packets,
            )
        });

        assert_eq!(drain.work_done, 0);
        assert!(drain.has_more);
    }

    #[def_test(serial)]
    fn device_contention_retains_rx_for_retry() {
        let (state, mut io) = network_with_ready_loopback_packet();
        let handle = state.with_interfaces(|interfaces| interfaces.devices()[0].clone());
        let mut packets = Vec::new();
        {
            let _device = handle.lock();
            let drain = state.with_interfaces(|interfaces| {
                io.drain_device_rx_budgeted(
                    interfaces.devices(),
                    MonotonicInstant::ORIGIN,
                    1,
                    &mut packets,
                )
            });
            assert_eq!(drain.work_done, 0);
            assert!(drain.has_more);
            assert!(drain.has_lock_contention);
            // Readiness remains conservative even with no RX capacity.
            let drain = state.with_interfaces(|interfaces| {
                io.drain_device_rx_budgeted(
                    interfaces.devices(),
                    MonotonicInstant::ORIGIN,
                    0,
                    &mut packets,
                )
            });
            assert!(drain.has_more);
        }
        let drain = state.with_interfaces(|interfaces| {
            io.drain_device_rx_budgeted(
                interfaces.devices(),
                MonotonicInstant::ORIGIN,
                1,
                &mut packets,
            )
        });
        assert_eq!(drain.work_done, 1);
        assert!(!drain.has_lock_contention);
        assert!(!drain.has_more);
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
