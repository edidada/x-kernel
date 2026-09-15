// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network service assembly and bounded poll-round coordination.
use alloc::{sync::Arc, vec::Vec};
use core::task::Waker;

use kerrno::KResult;
use khal::time::monotonic_time;
use kpoll::{PollContext, PollRegisterError, PollSet};
use ksync::Mutex;
use ktime_types::{MonotonicInstant, TimeSpan};
use smoltcp::wire::IpAddress as SmoltcpIpAddress;

use crate::{
    SOCKET_SET,
    datapath::{NetIo, io::RxDrain},
    dataplane::NetworkDataPlane,
    ip::IpAddress,
    network::{
        ipv4::{
            ingress::Ipv4Ingress,
            output::{Ipv4Output, Ipv4SendOptions},
        },
        state::NetworkState,
    },
    poller::{PollBudget, PollProgress},
    stack::{
        interface::Stack,
        time::{earliest_poll_deadline, has_immediate_timer_work, now},
    },
};
const POLL_ROUND_TIME_LIMIT: TimeSpan = TimeSpan::from_millis(1);
pub(crate) const POLL_TIME_CHECK_INTERVAL_WORK_ITEMS: usize = 32;

pub(crate) struct Service {
    pub(crate) stack: Stack,
    network: NetworkState,
    io: Mutex<NetIo>,
    ipv4_output: Ipv4Output,
    dataplane: NetworkDataPlane,
    pub(crate) ipv4_ingress: Ipv4Ingress,
    pub(crate) events: ServiceEvents,
}
pub(crate) struct ServiceEvents {
    poll: Arc<PollSet>,
}
impl ServiceEvents {
    fn new() -> Self {
        Self {
            poll: Arc::new(PollSet::new()),
        }
    }

    pub(crate) fn register(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        context.register(self.poll.as_ref())?;
        Ok(Waker::from(self.poll.clone()))
    }

    pub(crate) fn wake(&self) {
        self.poll.wake();
    }

    pub(crate) fn notify_timer(&self) {
        self.wake();
        crate::poller::network_poller().notify(crate::poller::PollReason::Timer);
    }
}
impl Service {
    pub(crate) fn new(state: NetworkState) -> Self {
        let mut io = NetIo::new();
        let mtu = state.effective_mtu();
        let stack = Stack::new(&mut io, mtu);
        let ipv4_ingress = Ipv4Ingress::new();
        let addrs = state.local_ipv4_addrs();
        ipv4_ingress.set_ipv4_addrs(&addrs);
        stack.set_ipv4_addrs(&addrs);
        Self {
            stack,
            network: state,
            io: Mutex::new(io),
            ipv4_output: Ipv4Output::new(),
            dataplane: NetworkDataPlane::new(),
            ipv4_ingress,
            events: ServiceEvents::new(),
        }
    }

    pub(crate) fn network(&self) -> &NetworkState {
        &self.network
    }

    pub(crate) fn with_io<R>(&self, use_fn: impl FnOnce(&mut NetIo) -> R) -> R {
        use_fn(&mut self.io.lock())
    }

    pub(crate) fn try_with_io<R>(&self, use_fn: impl FnOnce(&mut NetIo) -> R) -> Option<R> {
        let mut io = self.io.try_lock()?;
        Some(use_fn(&mut io))
    }

    /// Device RX takes the interface write lock first, then `NetIo`, matching
    /// configuration teardown. Both guards are released before ingress.
    pub(crate) fn try_drain_device_rx(
        &self,
        timestamp: MonotonicInstant,
        budget: usize,
        packets: &mut Vec<crate::buf::PacketBuf>,
    ) -> Option<RxDrain> {
        let mut interfaces = self.network.try_write_interfaces()?;
        let mut io = self.io.try_lock()?;
        let batch_budget = budget.min(io.stack_ingress_capacity());
        Some(io.drain_device_rx_budgeted(&mut interfaces, timestamp, batch_budget, packets))
    }

    /// Advances one bounded network round in task context without spinlocks.
    /// Queue and ingress contention retain batches and report immediate work.
    /// Device TX and configuration snapshots still take blocking interface
    /// locks. Pending reassembly expiry does not block TX.
    pub fn poll_budgeted(&self, budget: PollBudget) -> PollProgress {
        let round_started_at = monotonic_time();
        let device_timestamp = round_started_at;
        let mut control_packets = self.dataplane.control_tx_batch.lock();
        let mut rx_batch = self.dataplane.rx_batch.lock();
        let mut accepted_packets = self.dataplane.stack_ingress_batch.lock();

        let ipv4_expiration = self.ipv4_ingress.expire_fragments(
            device_timestamp,
            budget.timer_events,
            &mut control_packets,
        );
        let mut timer_events = ipv4_expiration.timer_events;
        if !self
            .dataplane
            .flush_ingress(self, &mut accepted_packets, &mut control_packets)
        {
            return deferred_poll_progress(0, 0, timer_events, false);
        }

        let tx_progress = self.dataplane.poll_tx_budgeted(
            &self.network,
            &self.io,
            &self.ipv4_output,
            device_timestamp,
            budget.tx_packets,
            round_started_at,
        );
        let mut tx_packets = tx_progress.packets;
        let mut has_reached_time_limit = tx_progress.has_reached_time_limit;
        let mut tx_capacity_changed = tx_progress.tx_capacity_changed;
        if tx_progress.has_lock_contention {
            return deferred_poll_progress(0, tx_packets, timer_events, tx_capacity_changed);
        }
        let mut tx_remaining = budget.tx_packets.saturating_sub(tx_packets);

        let rx_progress = self.dataplane.poll_rx_budgeted(
            self,
            round_started_at,
            if has_reached_time_limit {
                0
            } else {
                budget.rx_packets
            },
            &mut rx_batch,
            &mut accepted_packets,
            &mut control_packets,
        );
        let mut rx_packets = rx_progress.packets;
        has_reached_time_limit |= rx_progress.has_reached_time_limit;
        if rx_progress.has_lock_contention {
            return deferred_poll_progress(
                rx_packets,
                tx_packets,
                timer_events,
                tx_capacity_changed,
            );
        }

        let mtu = self.network.effective_mtu();
        let Some(mut sockets) = SOCKET_SET.inner.try_lock() else {
            return deferred_poll_progress(
                rx_packets,
                tx_packets,
                timer_events,
                tx_capacity_changed,
            );
        };
        let Some(mut io) = self.io.try_lock() else {
            return deferred_poll_progress(
                rx_packets,
                tx_packets,
                timer_events,
                tx_capacity_changed,
            );
        };
        let Some(stack_progress) = self.stack.poll(
            &mut io,
            mtu,
            &mut sockets,
            budget,
            round_started_at,
            has_reached_time_limit,
        ) else {
            return deferred_poll_progress(
                rx_packets,
                tx_packets,
                timer_events,
                tx_capacity_changed,
            );
        };
        has_reached_time_limit = stack_progress.has_reached_time_limit;
        let has_socket_state_change = stack_progress.has_socket_state_change;
        let protocol_deadline = stack_progress.protocol_deadline;
        let deferred_close_deadline = stack_progress.deferred_close_deadline;
        drop(io);
        drop(sockets);
        timer_events += usize::from(timer_events < budget.timer_events && has_socket_state_change);

        if has_reached_time_limit {
            tx_remaining = 0;
        }
        let tx_progress = self.dataplane.poll_tx_budgeted(
            &self.network,
            &self.io,
            &self.ipv4_output,
            device_timestamp,
            tx_remaining,
            round_started_at,
        );
        tx_packets += tx_progress.packets;
        tx_capacity_changed |= tx_progress.tx_capacity_changed;
        has_reached_time_limit |= tx_progress.has_reached_time_limit;
        if tx_progress.has_lock_contention {
            return deferred_poll_progress(
                rx_packets,
                tx_packets,
                timer_events,
                tx_capacity_changed,
            );
        }

        let rx_remaining = budget.rx_packets.saturating_sub(rx_packets);
        if !has_reached_time_limit && rx_remaining > 0 {
            let tail_progress = self.dataplane.poll_rx_budgeted(
                self,
                round_started_at,
                rx_remaining,
                &mut rx_batch,
                &mut accepted_packets,
                &mut control_packets,
            );
            rx_packets += tail_progress.packets;
            if tail_progress.has_lock_contention {
                return deferred_poll_progress(
                    rx_packets,
                    tx_packets,
                    timer_events,
                    tx_capacity_changed,
                );
            }
        }

        let next_poll = earliest_poll_deadline(protocol_deadline, deferred_close_deadline);
        let timer_current = now();
        let timer_has_more = has_immediate_timer_work(timer_current, next_poll);
        self.stack
            .update_protocol_deadline(timer_current, next_poll, &self.events);

        let Some(io) = self.io.try_lock() else {
            return deferred_poll_progress(
                rx_packets,
                tx_packets,
                timer_events,
                tx_capacity_changed,
            );
        };
        let ingress_has_more = io.has_stack_ingress();
        let tx_has_more = io.has_pending_tx();
        drop(io);
        let rx_has_more = self
            .network
            .with_interfaces(|interfaces| interfaces.has_rx_work());
        PollProgress {
            rx_packets,
            tx_packets,
            timer_events,
            tx_capacity_changed,
            has_more: rx_has_more
                || ingress_has_more
                || tx_has_more
                || timer_has_more
                || ipv4_expiration.has_more
                || self.ipv4_ingress.has_pending_timer(),
        }
    }

    /// Resolves the route before the queue preparation and enqueue transaction.
    /// Capacity failure preserves the caller's packet for retry.
    pub fn prepare_and_send_ipv4_packet(
        &self,
        bound_src: Option<IpAddress>,
        dst_addr: &IpAddress,
        oif: i32,
        allow_broadcast: bool,
        packet: &mut Option<Vec<u8>>,
        prepare: impl FnOnce(&mut [u8], IpAddress, Option<usize>) -> KResult,
    ) -> KResult {
        self.ipv4_output.prepare_and_send(
            &self.network,
            &self.io,
            Ipv4SendOptions {
                bound_src,
                dst_addr: *dst_addr,
                oif,
                allow_broadcast,
            },
            packet,
            prepare,
        )
    }

    pub(crate) fn get_source_address(&self, dst: &IpAddress) -> KResult<IpAddress> {
        self.network.select_source_address(dst)
    }

    pub(crate) fn get_smoltcp_source_address(
        &self,
        dst: &SmoltcpIpAddress,
    ) -> KResult<SmoltcpIpAddress> {
        self.network
            .select_source_address(&crate::ip::from_smoltcp_ip_address(*dst))
            .map(crate::ip::to_smoltcp_ip_address)
    }

    pub(crate) fn can_send_ip_packet(&self) -> bool {
        self.with_io(|io| io.can_enqueue_tx_packet())
    }

    pub(crate) fn register_rx_waker(
        &self,
        mask: u32,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        let waker = self.events.register(context)?;
        self.stack.refresh_deadline(&self.events);
        self.network.with_interfaces(|interfaces| {
            for (index, device) in interfaces.devices().iter().enumerate() {
                if mask & (1 << index) != 0 {
                    device.register_rx_waker(&waker, context)?;
                }
            }
            Ok(())
        })?;
        Ok(waker)
    }

    pub(crate) fn handle_timer_tick(&self) {
        let current_micros = u64::try_from(now().total_micros()).unwrap_or(0);
        let protocol_expired = self.stack.handle_timer_tick(current_micros);
        let reassembly_expired = self.ipv4_ingress.handle_timer_tick(current_micros);
        if protocol_expired || reassembly_expired {
            self.events.notify_timer();
        }
    }

    pub(crate) fn send_link_frame(&self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        self.network
            .with_interfaces_mut(|interfaces| interfaces.send_link_frame(ifindex, frame))
    }

    #[cfg(unittest)]
    pub(crate) fn rebuild_count_for_tests(&self) -> usize {
        self.stack
            .rebuild_count
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn rebuild_stack(&self) {
        let mtu = self.network.effective_mtu();
        self.with_io(|io| {
            self.stack.rebuild_interface(io, mtu, &self.events);
        });
    }

    pub(crate) fn repair_rx_cursor_after_device_removed(&self, removed_index: usize) {
        self.network.with_interfaces(|interfaces| {
            self.with_io(|io| {
                io.on_device_removed(interfaces, removed_index);
            });
        });
    }

    #[cfg(unittest)]
    pub(crate) fn replace_network_for_tests(&self, state: NetworkState) {
        self.network.replace_for_tests(state);
        *self.io.lock() = NetIo::new();
        let mtu = self.network.effective_mtu();
        self.with_io(|io| {
            self.stack.replace_for_tests(io, mtu);
        });
        let addrs = self.network.local_ipv4_addrs();
        self.ipv4_ingress.set_ipv4_addrs(&addrs);
        self.stack.set_ipv4_addrs(&addrs);
        self.events.wake();
    }
}
pub(crate) fn has_reached_poll_round_time_limit(
    round_started_at: MonotonicInstant,
    current: MonotonicInstant,
) -> bool {
    current.saturating_duration_since(round_started_at) >= POLL_ROUND_TIME_LIMIT
}
fn deferred_poll_progress(
    rx_packets: usize,
    tx_packets: usize,
    timer_events: usize,
    tx_capacity_changed: bool,
) -> PollProgress {
    PollProgress {
        rx_packets,
        tx_packets,
        timer_events,
        tx_capacity_changed,
        has_more: true,
    }
}

#[cfg(unittest)]
pub(crate) mod tests {
    use alloc::boxed::Box;
    use core::sync::atomic::Ordering;

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        device::{LoopbackDevice, interfaces::Ipv4AddrEntry},
        ip::{Ipv4Address, Ipv4Cidr},
        network::{ipv4, ipv4::ingress::IPV4_REASSEMBLY_TIMER_PENDING},
        router::ROUTE_SCOPE_HOST,
    };
    pub(crate) const TEST_POLL_BUDGET: PollBudget = PollBudget {
        rx_packets: 1,
        tx_packets: 1,
        timer_events: 1,
    };
    #[def_test]
    fn protocol_deadline_update_preserves_ipv4_reassembly_timer_source() {
        let service = Service::new(NetworkState::new());
        let reassembly_deadline = monotonic_time() + TimeSpan::from_secs(30);
        service
            .ipv4_ingress
            .update_reassembly_deadline(Some(reassembly_deadline));
        let reassembly_state = service
            .ipv4_ingress
            .reassembly_deadline_micros
            .load(Ordering::Acquire);

        service
            .stack
            .update_protocol_deadline(now(), None, &service.events);

        assert_eq!(
            service
                .ipv4_ingress
                .reassembly_deadline_micros
                .load(Ordering::Acquire),
            reassembly_state
        );
    }
    #[def_test]
    fn poll_round_time_limit_starts_at_one_millisecond() {
        let start = MonotonicInstant::from_span_since_origin(TimeSpan::from_millis(10));
        let before_limit = MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(10_999));
        let at_limit = MonotonicInstant::from_span_since_origin(TimeSpan::from_millis(11));

        assert!(!has_reached_poll_round_time_limit(start, before_limit));
        assert!(has_reached_poll_round_time_limit(start, at_limit));
    }
    #[def_test]
    fn poll_round_defers_packet_io_lock_contention() {
        let service = Service::new(NetworkState::new());
        let _io = service.io.lock();

        let progress = service.poll_budgeted(TEST_POLL_BUDGET);

        assert_eq!(progress.rx_packets, 0);
        assert_eq!(progress.tx_packets, 0);
        assert!(progress.has_more);
    }
    #[def_test(serial)]
    fn poll_round_dispatches_tx_when_due_ipv4_expiration_lock_is_contended() {
        let loopback_addr = Ipv4Address::new(127, 0, 0, 1);
        let router = NetworkState::new();
        let loopback = router.add_device(Box::new(LoopbackDevice::new()));
        router
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
        let service = Service::new(router);
        let dst = IpAddress::Ipv4(loopback_addr);
        let mtu = service.network.route_mtu(&dst);
        service
            .with_io(|io| {
                service
                    .ipv4_output
                    .send(io, mtu, packet, loopback as i32 + 1)
            })
            .unwrap();
        service
            .ipv4_ingress
            .reassembly_deadline_micros
            .store(IPV4_REASSEMBLY_TIMER_PENDING, Ordering::Release);
        let _ingress = service.ipv4_ingress.state.lock();

        let progress = service.poll_budgeted(TEST_POLL_BUDGET);

        assert_eq!(progress.tx_packets, 1);
        assert!(progress.has_more);
    }

    #[def_test(serial)]
    fn ingress_handoff_preserves_batches_on_destination_contention() {
        use alloc::vec;

        use crate::buf::{PacketBuf, PacketOwner};
        let service = Service::new(NetworkState::new());
        let mut accepted = vec![PacketBuf::from_ip_packet_vec(
            1,
            vec![0x45, 0, 0, 20],
            PacketOwner::DeviceRx,
        )];
        let mut control = vec![vec![0; 20]];
        {
            let _sockets = SOCKET_SET.inner.lock();
            assert!(
                !service
                    .dataplane
                    .flush_ingress(&service, &mut accepted, &mut control)
            );
        }
        assert_eq!(accepted.len(), 1);
        assert_eq!(control.len(), 1);
        {
            let _io = service.io.lock();
            assert!(
                !service
                    .dataplane
                    .flush_ingress(&service, &mut accepted, &mut control)
            );
        }
        assert_eq!(accepted.len(), 1);
        assert_eq!(control.len(), 1);
        assert!(
            service
                .dataplane
                .flush_ingress(&service, &mut accepted, &mut control)
        );
        assert!(accepted.is_empty());
        assert!(control.is_empty());
        assert!(service.with_io(|io| io.has_stack_ingress()));
        assert!(service.with_io(|io| io.has_pending_tx()));
    }
}
