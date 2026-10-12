// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reusable packet batches and native/stack data-plane handoff.
use alloc::{collections::VecDeque, vec::Vec};

use khal::time::monotonic_time;
use ksync::Mutex;
use ktime_types::MonotonicInstant;

use crate::{
    SOCKET_SET,
    buf::{PacketBuf, PacketOwner},
    io::NetIo,
    network::{
        ipv4::{
            self,
            ingress::{Ipv4ExpirationProgress, Ipv4Ingress},
            output::Ipv4Output,
        },
        state::{NetworkState, TxBatch},
    },
    service::{POLL_TIME_CHECK_INTERVAL_WORK_ITEMS, Service, has_reached_poll_round_time_limit},
    stack::interface::Stack,
};

const RX_INGRESS_BATCH_PACKETS: usize = 64;

#[derive(Default)]
pub(crate) struct DeviceRxProgress {
    pub packets: usize,
    pub has_reached_time_limit: bool,
    /// RX was deferred by lock contention or stack-ingress capacity.
    pub is_deferred: bool,
}

#[derive(Default)]
pub(crate) struct DeviceTxProgress {
    pub packets: usize,
    pub has_reached_time_limit: bool,
    pub has_lock_contention: bool,
    pub tx_capacity_changed: bool,
}

/// Reusable packet batches with stage-scoped locking.
///
/// When combined, batch locks follow control, RX, then stack-ingress order.
pub(crate) struct NetworkDataPlane {
    rx_batch: Mutex<Vec<PacketBuf>>,
    stack_ingress_batch: Mutex<Vec<PacketBuf>>,
    control_tx_batch: Mutex<Vec<Vec<u8>>>,
    /// Accessed only by the sole poll-round executor, including readiness checks.
    tx_batch: Mutex<VecDeque<PacketBuf>>,
}

impl NetworkDataPlane {
    pub(crate) fn new() -> Self {
        Self {
            rx_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            stack_ingress_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            control_tx_batch: Mutex::new(Vec::new()),
            tx_batch: Mutex::new(VecDeque::with_capacity(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS)),
        }
    }

    /// Reports retained TX while the caller owns protocol progression.
    pub(crate) fn has_pending_tx(&self) -> bool {
        !self.tx_batch.lock().is_empty()
    }

    pub(crate) fn expire_ipv4_fragments(
        &self,
        ingress: &Ipv4Ingress,
        timestamp: MonotonicInstant,
        timer_budget: usize,
    ) -> Ipv4ExpirationProgress {
        let mut control = self.control_tx_batch.lock();
        ingress.expire_fragments(timestamp, timer_budget, &mut control)
    }

    pub(crate) fn flush_retained_ingress(&self, service: &Service) -> bool {
        let mut control = self.control_tx_batch.lock();
        let mut accepted = self.stack_ingress_batch.lock();
        self.flush_ingress(service, &mut accepted, &mut control)
    }

    pub(crate) fn poll_rx_budgeted(
        &self,
        service: &Service,
        round_started_at: MonotonicInstant,
        budget: usize,
    ) -> DeviceRxProgress {
        let mut control_packets = self.control_tx_batch.lock();
        let mut rx_batch = self.rx_batch.lock();
        let mut accepted_packets = self.stack_ingress_batch.lock();
        self.poll_rx_budgeted_locked(
            service,
            round_started_at,
            budget,
            &mut rx_batch,
            &mut accepted_packets,
            &mut control_packets,
        )
    }

    fn poll_rx_budgeted_locked(
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
            progress.is_deferred = true;
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
                progress.is_deferred = true;
                break;
            };
            progress.packets += drain.work_done;
            progress.is_deferred |= drain.has_lock_contention;
            packets_since_time_check = packets_since_time_check.saturating_add(drain.work_done);

            if let Some((ifindex, error)) = &drain.error {
                warn!("network RX poll failed on ifindex {ifindex}: {error:?}");
            }

            if !self.process_rx_batch(
                service,
                device_timestamp,
                rx_batch,
                accepted_packets,
                control_packets,
            ) || !self.flush_ingress(service, accepted_packets, control_packets)
            {
                progress.is_deferred = true;
                break;
            }
            if drain.error.is_some()
                || drain.has_lock_contention
                || !drain.has_more
                || drain.work_done == 0
            {
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

    /// Hands off complete batches, retaining them on lock or RX-capacity failure.
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
                    // Reserve the whole batch before publishing listener state.
                    if accepted_packets.len() > io.available_stack_ingress_slots() {
                        return false;
                    }
                    Stack::prepare_ingress(accepted_packets, sockets);
                    if io.enqueue_stack_ingress(accepted_packets).is_err() {
                        return false;
                    }
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

    fn dequeue_tx_batch(io: &mut NetIo, budget: usize, packets: &mut VecDeque<PacketBuf>) -> bool {
        debug_assert!(packets.is_empty());
        let capacity_freed = io.pull_tx_packets(budget, packets);
        for packet in packets.iter_mut() {
            packet.set_owner(PacketOwner::DeviceTx);
        }
        capacity_freed
    }

    fn dispatch_dequeued(
        batch: &mut TxBatch<'_>,
        ipv4_output: &Ipv4Output,
        packets: &mut VecDeque<PacketBuf>,
        timestamp: MonotonicInstant,
        budget: usize,
    ) -> (usize, bool) {
        let mut dispatched = 0;
        while dispatched < budget {
            let Some(packet) = packets.pop_front() else {
                break;
            };
            let result = match packet.network_packet().and_then(ipv4::ip_version) {
                Some(4) => ipv4_output.dispatch(batch, packet, timestamp),
                Some(6) => crate::stack::ipv6_output::dispatch(batch, packet, timestamp),
                _ => {
                    debug!("Dropping packet with invalid IP version");
                    Ok(false)
                }
            };
            if let Err(packet) = result {
                packets.push_front(packet);
                return (dispatched, true);
            }
            dispatched += 1;
        }
        (dispatched, false)
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
            if packets.is_empty() {
                let Some(mut io) = io.try_lock() else {
                    progress.has_lock_contention = true;
                    break;
                };
                progress.tx_capacity_changed |=
                    Self::dequeue_tx_batch(&mut io, chunk, &mut packets);
            }
            if packets.is_empty() {
                break;
            }
            let Some((dispatched, has_lock_contention)) = state.try_with_tx_batch(|batch| {
                Self::dispatch_dequeued(batch, ipv4_output, &mut packets, timestamp, chunk)
            }) else {
                progress.has_lock_contention = true;
                break;
            };
            progress.packets += dispatched;
            if has_lock_contention {
                progress.has_lock_contention = true;
                break;
            }
            if dispatched < chunk {
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
        ip::{IpAddress, Ipv4Address, Ipv4Cidr},
        router::ROUTE_SCOPE_HOST,
        service::tests::TEST_POLL_BUDGET,
    };
    #[def_test]
    fn contended_tx_retains_order_and_obeys_a_smaller_retry_budget() {
        use core::sync::atomic::{AtomicUsize, Ordering};

        use crate::device::test_device::TestDevice;

        let sends = Arc::new(AtomicUsize::new(0));
        let state = NetworkState::new();
        let dev = state.add_device(Box::new(TestDevice::ethernet(1, 1500).with_send_hook({
            let sends = sends.clone();
            move || {
                sends.fetch_add(1, Ordering::Relaxed);
            }
        })));
        let src = Ipv4Address::new(192, 0, 2, 1);
        state
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(src, 24).unwrap(),
                scope: crate::router::ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            })
            .unwrap();
        let packet_fn = |marker| {
            PacketBuf::from_ip_packet_vec(
                0,
                ipv4::build_ipv4_packet(
                    src,
                    Ipv4Address::new(192, 0, 2, 2),
                    ipv4::PROTOCOL_ICMP,
                    64,
                    &[marker],
                )
                .unwrap(),
                PacketOwner::Ipv4Stack,
            )
        };
        let first = packet_fn(1);
        let second = packet_fn(2);
        let mut io = NetIo::new();
        io.enqueue_tx([first.clone(), second.clone()].into_iter());
        let io = Mutex::new(io);
        let plane = NetworkDataPlane::new();
        let output = Ipv4Output::new();
        let poll_fn = |budget| {
            let current = monotonic_time();
            plane.poll_tx_budgeted(&state, &io, &output, current, budget, current)
        };
        let dst = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 2));
        let route = state.resolve(&dst).unwrap();
        // Device-mutex contention retains the batch; inventory reads stay available.
        let progress = {
            let _device = route.lock_egress();
            poll_fn(2)
        };
        assert!(progress.has_lock_contention);
        assert!(progress.tx_capacity_changed);
        assert_eq!(progress.packets, 0);
        assert_eq!(sends.load(Ordering::Relaxed), 0);
        {
            let pending = plane.tx_batch.lock();
            assert_eq!(pending.len(), 2);
            assert_eq!(pending[0].network_packet(), first.network_packet());
            assert_eq!(pending[1].network_packet(), second.network_packet());
        }
        io.lock().enqueue_tx(core::iter::once(packet_fn(3)));
        let progress = poll_fn(1);
        assert_eq!(progress.packets, 1);
        assert!(!progress.tx_capacity_changed);
        assert!(plane.has_pending_tx());
        {
            let pending = plane.tx_batch.lock();
            assert_eq!(pending[0].network_packet(), second.network_packet());
        }
        assert!(io.lock().has_pending_tx());
        assert_eq!(poll_fn(1).packets, 1);
        assert!(!plane.has_pending_tx());
        assert_eq!(poll_fn(1).packets, 1);
        assert_eq!(sends.load(Ordering::Relaxed), 3);
    }

    #[def_test]
    fn control_dispatch_does_not_free_data_tx_capacity() {
        let state = NetworkState::new();
        let mut io = NetIo::new();
        let output = Ipv4Output::new();
        io.enqueue_control_tx(vec![0; 20]).unwrap();
        let could_admit_before = io.can_admit_ipv4_egress(1);
        let io = Mutex::new(io);
        let current = monotonic_time();
        let progress =
            NetworkDataPlane::new().poll_tx_budgeted(&state, &io, &output, current, 1, current);

        assert_eq!(progress.packets, 1);
        assert!(!progress.tx_capacity_changed);
        assert_eq!(could_admit_before, io.lock().can_admit_ipv4_egress(1));
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
        let io = Mutex::new(io);
        let current = monotonic_time();
        let progress =
            NetworkDataPlane::new().poll_tx_budgeted(&state, &io, &output, current, 1, current);

        assert_eq!(progress.packets, 1);
        assert!(progress.tx_capacity_changed);
        assert!(io.lock().can_admit_ipv4_egress(1));
    }

    #[def_test(serial)]
    fn tx_dispatch_exposes_loopback_rx_in_the_same_round() {
        let loopback_addr = Ipv4Address::new(127, 0, 0, 1);
        let state = NetworkState::new();
        let loopback = state.add_device(Box::new(LoopbackDevice::new()));
        state
            .add_ipv4_addr(Ipv4AddrEntry {
                dev: loopback,
                addr: Ipv4Cidr::new(loopback_addr, 8).unwrap(),
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
        let rx_progress = state.with_interfaces(|interfaces| {
            io.lock().drain_device_rx_budgeted(
                interfaces.devices(),
                round_started_at,
                TEST_POLL_BUDGET.rx_packets,
                &mut packets,
            )
        });

        assert_eq!(tx_progress.packets, 1);
        assert_eq!(rx_progress.work_done, 1);
    }
}
