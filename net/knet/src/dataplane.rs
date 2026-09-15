// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reusable packet batches and native/stack data-plane handoff.
use alloc::vec::Vec;

use khal::time::monotonic_time;
use ksync::Mutex;
use ktime_types::MonotonicInstant;

use crate::{
    SOCKET_SET,
    buf::{PacketBuf, PacketOwner},
    datapath::NetIo,
    network::{ipv4, ipv4::output::Ipv4Output, state::NetworkState},
    service::{POLL_TIME_CHECK_INTERVAL_WORK_ITEMS, Service, has_reached_poll_round_time_limit},
    stack::interface::Stack,
};

const RX_INGRESS_BATCH_PACKETS: usize = 64;

#[derive(Default)]
pub(crate) struct DeviceRxProgress {
    pub packets: usize,
    pub has_reached_time_limit: bool,
    pub has_lock_contention: bool,
}

#[derive(Default)]
pub(crate) struct DeviceTxProgress {
    pub packets: usize,
    pub has_reached_time_limit: bool,
    pub has_lock_contention: bool,
    pub tx_capacity_changed: bool,
}

pub(crate) struct NetworkDataPlane {
    pub(crate) rx_batch: Mutex<Vec<PacketBuf>>,
    pub(crate) stack_ingress_batch: Mutex<Vec<PacketBuf>>,
    pub(crate) control_tx_batch: Mutex<Vec<Vec<u8>>>,
    tx_batch: Mutex<Vec<PacketBuf>>,
}

impl NetworkDataPlane {
    pub(crate) fn new() -> Self {
        Self {
            rx_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            stack_ingress_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            control_tx_batch: Mutex::new(Vec::new()),
            tx_batch: Mutex::new(Vec::with_capacity(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS)),
        }
    }

    pub(crate) fn poll_rx_budgeted(
        &self,
        service: &Service,
        round_started_at: MonotonicInstant,
        budget: usize,
        rx_batch: &mut Vec<PacketBuf>,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> DeviceRxProgress {
        let device_timestamp = round_started_at;
        let mut progress = DeviceRxProgress::default();
        let mut packets_since_time_check = 0;

        if !self.process_rx_batch(
            service,
            device_timestamp,
            rx_batch,
            accepted_packets,
            control_packets,
        ) || !self.flush_ingress(service, accepted_packets, control_packets)
        {
            progress.has_lock_contention = true;
            return progress;
        }

        while progress.packets < budget {
            if packets_since_time_check >= POLL_TIME_CHECK_INTERVAL_WORK_ITEMS {
                packets_since_time_check = 0;
                if has_reached_poll_round_time_limit(round_started_at, monotonic_time()) {
                    progress.has_reached_time_limit = true;
                    break;
                }
            }

            let drain = service.try_drain_device_rx(
                device_timestamp,
                budget
                    .saturating_sub(progress.packets)
                    .min(RX_INGRESS_BATCH_PACKETS)
                    .min(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS),
                rx_batch,
            );
            let Some(drain) = drain else {
                progress.has_lock_contention = true;
                break;
            };
            progress.packets += drain.work_done;
            packets_since_time_check = packets_since_time_check.saturating_add(drain.work_done);

            if !self.process_rx_batch(
                service,
                device_timestamp,
                rx_batch,
                accepted_packets,
                control_packets,
            ) || !self.flush_ingress(service, accepted_packets, control_packets)
            {
                progress.has_lock_contention = true;
                break;
            }
            if !drain.has_more || drain.work_done == 0 {
                break;
            }
        }

        if progress.packets > 0
            && has_reached_poll_round_time_limit(round_started_at, monotonic_time())
        {
            progress.has_reached_time_limit = true;
        }
        progress
    }

    fn process_rx_batch(
        &self,
        service: &Service,
        device_timestamp: MonotonicInstant,
        rx_batch: &mut Vec<PacketBuf>,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> bool {
        if rx_batch.is_empty() {
            return true;
        }

        let Some(mut ingress) = service.ipv4_ingress.state.try_lock() else {
            return false;
        };
        for packet in rx_batch.drain(..) {
            let Some(ip_packet) = packet.network_packet() else {
                continue;
            };
            match ipv4::ip_version(ip_packet) {
                Some(4) => ingress.process_packet(
                    device_timestamp,
                    packet,
                    accepted_packets,
                    control_packets,
                ),
                Some(6) => accepted_packets.push(packet),
                _ => debug!("Dropping packet with invalid IP version"),
            }
        }
        let next_deadline = ingress.ipv4_reassembly_deadline();
        service
            .ipv4_ingress
            .update_reassembly_deadline(next_deadline);
        drop(ingress);
        true
    }

    pub(crate) fn flush_ingress(
        &self,
        service: &Service,
        accepted_packets: &mut Vec<PacketBuf>,
        control_packets: &mut Vec<Vec<u8>>,
    ) -> bool {
        if accepted_packets.is_empty() && control_packets.is_empty() {
            return true;
        }

        let mut sockets = if accepted_packets.is_empty() {
            None
        } else {
            let Some(sockets) = SOCKET_SET.inner.try_lock() else {
                return false;
            };
            Some(sockets)
        };
        service
            .try_with_io(|io| {
                if let Some(sockets) = &mut sockets {
                    // Acquire both destinations before listener preparation mutates the
                    // socket set. A failed nonblocking acquisition leaves the complete
                    // batch available for an identical retry in a later poll round.
                    Stack::prepare_ingress(accepted_packets, sockets);
                    io.enqueue_stack_ingress(accepted_packets);
                }
                for packet in control_packets.drain(..) {
                    if let Err(err) = io.enqueue_control_tx(packet) {
                        warn!("Dropping network control packet: {}", err);
                    }
                }
                true
            })
            .unwrap_or(false)
    }

    fn dequeue_tx_batch(io: &mut NetIo, budget: usize, packets: &mut Vec<PacketBuf>) -> bool {
        debug_assert!(packets.is_empty());
        let capacity_before = io.available_data_tx_slots();
        while packets.len() < budget {
            let Some(mut packet) = io.dequeue_tx() else {
                break;
            };
            packet.set_owner(PacketOwner::DeviceTx);
            packets.push(packet);
        }
        // Capture freed capacity in the dequeue critical section so another
        // producer or lock contender cannot hide the TX wakeup obligation.
        io.available_data_tx_slots() > capacity_before
    }

    fn dispatch_dequeued(
        state: &NetworkState,
        ipv4_output: &Ipv4Output,
        packets: &mut Vec<PacketBuf>,
        timestamp: MonotonicInstant,
    ) {
        for packet in packets.drain(..) {
            let Some(ip_packet) = packet.network_packet() else {
                continue;
            };
            match ipv4::ip_version(ip_packet) {
                Some(4) => {
                    ipv4_output.dispatch(state, packet, timestamp);
                }
                Some(6) => {
                    crate::stack::ipv6_output::dispatch(state, packet, timestamp);
                }
                _ => debug!("Dropping packet with invalid IP version"),
            }
        }
    }

    pub(crate) fn poll_tx_budgeted(
        &self,
        state: &NetworkState,
        io: &Mutex<NetIo>,
        ipv4_output: &Ipv4Output,
        timestamp: MonotonicInstant,
        budget: usize,
        round_started_at: MonotonicInstant,
    ) -> DeviceTxProgress {
        let mut progress = DeviceTxProgress::default();
        let mut packets = self.tx_batch.lock();

        while progress.packets < budget {
            let chunk = (budget - progress.packets).min(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS);
            {
                let Some(mut io) = io.try_lock() else {
                    progress.has_lock_contention = true;
                    break;
                };
                progress.tx_capacity_changed |=
                    Self::dequeue_tx_batch(&mut io, chunk, &mut packets);
            }
            let dequeued = packets.len();
            Self::dispatch_dequeued(state, ipv4_output, &mut packets, timestamp);
            progress.packets += dequeued;
            if dequeued < chunk {
                break;
            }
            if has_reached_poll_round_time_limit(round_started_at, monotonic_time()) {
                progress.has_reached_time_limit = true;
                break;
            }
        }

        progress
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, sync::Arc, vec};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        device::{LoopbackDevice, interfaces::Ipv4AddrEntry},
        ip::{Ipv4Address, Ipv4Cidr},
        router::ROUTE_SCOPE_HOST,
        service::tests::TEST_POLL_BUDGET,
    };
    #[def_test]
    fn control_dispatch_does_not_free_data_tx_capacity() {
        let state = NetworkState::new();
        let mut io = NetIo::new();
        let output = Ipv4Output::new();
        io.enqueue_control_tx(vec![0; 20]).unwrap();
        let capacity_before = io.available_data_tx_slots();
        let io = Mutex::new(io);
        let current = monotonic_time();
        let progress =
            NetworkDataPlane::new().poll_tx_budgeted(&state, &io, &output, current, 1, current);

        assert_eq!(progress.packets, 1);
        assert!(!progress.tx_capacity_changed);
        assert_eq!(io.lock().available_data_tx_slots(), capacity_before);
    }
    #[def_test]
    fn data_dispatch_frees_data_tx_capacity() {
        let state = NetworkState::new();
        let mut io = NetIo::new();
        let output = Ipv4Output::new();
        io.enqueue_tx(core::iter::once(PacketBuf::from_ip_packet_vec(
            0,
            vec![0; 20],
            PacketOwner::Ipv4Stack,
        )));
        let capacity_before = io.available_data_tx_slots();
        let io = Mutex::new(io);
        let current = monotonic_time();
        let progress =
            NetworkDataPlane::new().poll_tx_budgeted(&state, &io, &output, current, 1, current);

        assert_eq!(progress.packets, 1);
        assert!(progress.tx_capacity_changed);
        assert_eq!(io.lock().available_data_tx_slots(), capacity_before + 1);
    }

    #[def_test]
    fn tx_capacity_notification_survives_refill_during_device_send() {
        use crate::device::test_device::TestDevice;

        let io = Arc::new(Mutex::new(NetIo::new()));
        let state = NetworkState::new();
        let dev = state.add_device(Box::new(TestDevice::ethernet(1, 1500).with_send_hook({
            let io = io.clone();
            move || {
                // A producer can refill the freed slot during device I/O.
                let mut io = io
                    .try_lock()
                    .expect("device send must release the queue lock");
                io.enqueue_tx(core::iter::once(PacketBuf::from_ip_packet_vec(
                    0,
                    vec![0; 20],
                    PacketOwner::Ipv4Stack,
                )));
            }
        })));
        let src = Ipv4Address::new(192, 0, 2, 1);
        let dst = Ipv4Address::new(192, 0, 2, 2);
        state
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(src, 24),
                scope: crate::router::ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            })
            .unwrap();
        let packet = ipv4::build_ipv4_packet(src, dst, ipv4::PROTOCOL_ICMP, 64, &[]).unwrap();
        {
            let mut io = io.lock();
            let capacity = io.available_data_tx_slots();
            io.enqueue_tx(
                (0..capacity).map(|_| {
                    PacketBuf::from_ip_packet_vec(0, packet.clone(), PacketOwner::Ipv4Stack)
                }),
            );
        }
        let current = monotonic_time();
        let progress = NetworkDataPlane::new().poll_tx_budgeted(
            &state,
            &io,
            &Ipv4Output::new(),
            current,
            1,
            current,
        );

        assert_eq!(progress.packets, 1);
        assert!(progress.tx_capacity_changed);
        assert_eq!(io.lock().available_data_tx_slots(), 0);
    }
    #[def_test(serial)]
    fn tx_dispatch_exposes_loopback_rx_in_the_same_round() {
        let loopback_addr = Ipv4Address::new(127, 0, 0, 1);
        let state = NetworkState::new();
        let loopback = state.add_device(Box::new(LoopbackDevice::new()));
        state
            .add_ipv4_addr(Ipv4AddrEntry {
                dev: loopback,
                addr: Ipv4Cidr::new(loopback_addr, 8),
                scope: ROUTE_SCOPE_HOST,
                broadcast: None,
            })
            .unwrap();
        let packet =
            ipv4::build_ipv4_packet(loopback_addr, loopback_addr, ipv4::PROTOCOL_ICMP, 64, &[])
                .unwrap();
        let mut io = NetIo::new();
        let output = Ipv4Output::new();
        output
            .send(&mut io, Some(65536), packet, loopback as i32 + 1)
            .unwrap();
        let round_started_at = monotonic_time();
        let io = Mutex::new(io);

        let tx_progress = NetworkDataPlane::new().poll_tx_budgeted(
            &state,
            &io,
            &output,
            round_started_at,
            TEST_POLL_BUDGET.tx_packets,
            round_started_at,
        );
        let mut packets = Vec::new();
        let rx_progress = state.with_interfaces_mut(|interfaces| {
            io.lock().drain_device_rx_budgeted(
                interfaces,
                round_started_at,
                TEST_POLL_BUDGET.rx_packets,
                &mut packets,
            )
        });

        assert_eq!(tx_progress.packets, 1);
        assert_eq!(rx_progress.work_done, 1);
    }
}
