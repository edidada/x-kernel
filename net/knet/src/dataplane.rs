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

pub(crate) struct NetworkDataPlane {
    pub(crate) rx_batch: Mutex<Vec<PacketBuf>>,
    pub(crate) stack_ingress_batch: Mutex<Vec<PacketBuf>>,
    pub(crate) control_tx_batch: Mutex<Vec<Vec<u8>>>,
}

impl NetworkDataPlane {
    pub(crate) fn new() -> Self {
        Self {
            rx_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            stack_ingress_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            control_tx_batch: Mutex::new(Vec::new()),
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

            let drain = service.try_with_network(|state, io| {
                let batch_budget = budget
                    .saturating_sub(progress.packets)
                    .min(RX_INGRESS_BATCH_PACKETS)
                    .min(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS)
                    .min(io.stack_ingress_capacity());
                if batch_budget == 0 {
                    return None;
                }
                Some(io.drain_device_rx_budgeted(
                    &mut state.interfaces,
                    device_timestamp,
                    batch_budget,
                    rx_batch,
                ))
            });
            let Some(drain) = drain else {
                progress.has_lock_contention = true;
                break;
            };
            let Some(drain) = drain else {
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
            .try_with_network(|_state, io| {
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

    pub(crate) fn dispatch_tx_batch(
        state: &mut NetworkState,
        io: &mut NetIo,
        ipv4_output: &mut Ipv4Output,
        timestamp: MonotonicInstant,
        budget: usize,
    ) -> (usize, bool) {
        if budget == 0 {
            return (0, io.has_pending_tx());
        }

        let mut work_done = 0;
        let mut poll_next = false;
        while work_done < budget {
            let Some(mut packet) = io.dequeue_tx() else {
                break;
            };
            work_done += 1;
            packet.set_owner(PacketOwner::DeviceTx);
            let Some(ip_packet) = packet.network_packet() else {
                continue;
            };
            match ipv4::ip_version(ip_packet) {
                Some(4) => {
                    poll_next |= ipv4_output.dispatch(state, io, packet, timestamp);
                }
                Some(6) => {
                    poll_next |= crate::stack::ipv6_output::dispatch(state, io, packet, timestamp);
                }
                _ => debug!("Dropping packet with invalid IP version"),
            }
        }
        (work_done, poll_next || io.has_pending_tx())
    }

    pub(crate) fn poll_tx_budgeted(
        state: &mut NetworkState,
        io: &mut NetIo,
        ipv4_output: &mut Ipv4Output,
        timestamp: MonotonicInstant,
        budget: usize,
        round_started_at: MonotonicInstant,
    ) -> (usize, bool) {
        let mut work_done = 0;
        let mut remaining = budget;
        let mut has_reached_time_limit = false;

        while remaining > 0 {
            let chunk = remaining.min(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS);
            let (dispatched, _) = Self::dispatch_tx_batch(state, io, ipv4_output, timestamp, chunk);
            work_done += dispatched;
            remaining = remaining.saturating_sub(dispatched);
            if dispatched < chunk {
                break;
            }
            if has_reached_poll_round_time_limit(round_started_at, monotonic_time()) {
                has_reached_time_limit = true;
                break;
            }
        }

        (work_done, has_reached_time_limit)
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{boxed::Box, vec};

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{
        device::{LoopbackDevice, interfaces::Ipv4AddrEntry},
        ip::{Ipv4Address, Ipv4Cidr},
        router::ROUTE_SCOPE_HOST,
        service::tests::TEST_POLL_BUDGET,
    };
    #[def_test]
    fn control_dispatch_does_not_free_data_tx_capacity() {
        let mut state = NetworkState::new();
        let mut io = NetIo::new();
        let mut output = Ipv4Output::new();
        io.enqueue_control_tx(vec![0; 20]).unwrap();
        let capacity_before = io.available_data_tx_slots();

        let (work_done, _) = NetworkDataPlane::dispatch_tx_batch(
            &mut state,
            &mut io,
            &mut output,
            MonotonicInstant::ORIGIN,
            1,
        );

        assert_eq!(work_done, 1);
        assert_eq!(io.available_data_tx_slots(), capacity_before);
    }
    #[def_test]
    fn data_dispatch_frees_data_tx_capacity() {
        let mut state = NetworkState::new();
        let mut io = NetIo::new();
        let mut output = Ipv4Output::new();
        io.enqueue_tx(core::iter::once(PacketBuf::from_ip_packet_vec(
            0,
            vec![0; 20],
            PacketOwner::Ipv4Stack,
        )));
        let capacity_before = io.available_data_tx_slots();

        let (work_done, _) = NetworkDataPlane::dispatch_tx_batch(
            &mut state,
            &mut io,
            &mut output,
            MonotonicInstant::ORIGIN,
            1,
        );

        assert_eq!(work_done, 1);
        assert_eq!(io.available_data_tx_slots(), capacity_before + 1);
    }
    #[def_test(serial)]
    fn tx_dispatch_exposes_loopback_rx_in_the_same_round() {
        let loopback_addr = Ipv4Address::new(127, 0, 0, 1);
        let mut state = NetworkState::new();
        let loopback = state.interfaces.add_device(Box::new(LoopbackDevice::new()));
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
        let mut output = Ipv4Output::new();
        output
            .send(&state, &mut io, packet, loopback as i32 + 1)
            .unwrap();
        let round_started_at = monotonic_time();

        let (tx_packets, _) = NetworkDataPlane::poll_tx_budgeted(
            &mut state,
            &mut io,
            &mut output,
            round_started_at,
            TEST_POLL_BUDGET.tx_packets,
            round_started_at,
        );
        let mut packets = Vec::new();
        let rx_progress = io.drain_device_rx_budgeted(
            &mut state.interfaces,
            round_started_at,
            TEST_POLL_BUDGET.rx_packets,
            &mut packets,
        );

        assert_eq!(tx_packets, 1);
        assert_eq!(rx_progress.work_done, 1);
    }
}
