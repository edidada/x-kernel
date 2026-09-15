// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network service assembly and bounded poll-round coordination.
use alloc::{sync::Arc, vec::Vec};
use core::task::Waker;

use kerrno::{KResult, LinuxError};
use khal::time::monotonic_time;
use kpoll::{PollContext, PollRegisterError, PollSet};
use ksync::Mutex;
use ktime_types::{MonotonicInstant, TimeSpan};
use smoltcp::wire::IpAddress as SmoltcpIpAddress;

use crate::{
    SOCKET_SET,
    datapath::NetIo,
    dataplane::NetworkDataPlane,
    device::{LinkSendSnapshot, LinkSnapshot},
    ip::IpAddress,
    network::{
        ipv4::{
            ingress::Ipv4Ingress,
            output::{Ipv4Output, Ipv4SendOptions},
        },
        state::NetworkState,
    },
    poller::{PollBudget, PollProgress},
    router::Rule,
    stack::{
        interface::Stack,
        time::{earliest_poll_deadline, has_immediate_timer_work, now},
    },
};
const POLL_ROUND_TIME_LIMIT: TimeSpan = TimeSpan::from_millis(1);
pub(crate) const POLL_TIME_CHECK_INTERVAL_WORK_ITEMS: usize = 32;
/// Transaction storage; behavior belongs to the three owners inside it.
struct NetworkRuntime {
    pub state: NetworkState,
    pub io: NetIo,
    pub ipv4_output: Ipv4Output,
}
pub(crate) struct Service {
    pub(crate) stack: Stack,
    network: Mutex<NetworkRuntime>,
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
        let stack = Stack::new(&mut io, state.interfaces.effective_mtu());
        let ipv4_ingress = Ipv4Ingress::new();
        let addrs = state.interfaces.local_ipv4_addrs();
        ipv4_ingress.set_ipv4_addrs(&addrs);
        stack.set_ipv4_addrs(&addrs);
        Self {
            stack,
            network: Mutex::new(NetworkRuntime {
                state,
                io,
                ipv4_output: Ipv4Output::new(),
            }),
            dataplane: NetworkDataPlane::new(),
            ipv4_ingress,
            events: ServiceEvents::new(),
        }
    }

    /// Borrows inventory and transport under the shared network transaction.
    pub(crate) fn with_network<R>(
        &self,
        use_fn: impl FnOnce(&mut NetworkState, &mut NetIo) -> R,
    ) -> R {
        let mut guard = self.network.lock();
        let NetworkRuntime { state, io, .. } = &mut *guard;
        use_fn(state, io)
    }

    /// Leaves all handoff state untouched when the transaction is contended.
    pub(crate) fn try_with_network<R>(
        &self,
        use_fn: impl FnOnce(&mut NetworkState, &mut NetIo) -> R,
    ) -> Option<R> {
        let mut guard = self.network.try_lock()?;
        let NetworkRuntime { state, io, .. } = &mut *guard;
        Some(use_fn(state, io))
    }

    /// Advances one bounded network round in task context without spinlocks.
    /// Shared progression uses `try_lock`; contention retains batches and
    /// reports immediate work. Pending reassembly expiry does not block TX.
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

        let Some(mut network_guard) = self.network.try_lock() else {
            return deferred_poll_progress(0, 0, timer_events, false);
        };
        let network = &mut *network_guard;
        let tx_capacity_before = network.io.available_data_tx_slots();
        let (mut tx_packets, mut has_reached_time_limit) = NetworkDataPlane::poll_tx_budgeted(
            &mut network.state,
            &mut network.io,
            &mut network.ipv4_output,
            device_timestamp,
            budget.tx_packets,
            round_started_at,
        );
        let mut tx_capacity_changed = network.io.available_data_tx_slots() > tx_capacity_before;
        drop(network_guard);
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

        let Some(mut sockets) = SOCKET_SET.inner.try_lock() else {
            return deferred_poll_progress(
                rx_packets,
                tx_packets,
                timer_events,
                tx_capacity_changed,
            );
        };
        let Some(mut network_guard) = self.network.try_lock() else {
            return deferred_poll_progress(
                rx_packets,
                tx_packets,
                timer_events,
                tx_capacity_changed,
            );
        };
        let network = &mut *network_guard;
        let Some(stack_progress) = self.stack.poll(
            &mut network.io,
            network.state.interfaces.effective_mtu(),
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
        drop(sockets);
        timer_events += usize::from(timer_events < budget.timer_events && has_socket_state_change);

        if has_reached_time_limit {
            tx_remaining = 0;
        }
        let tx_capacity_before = network.io.available_data_tx_slots();
        let (dispatched, tx_time_limit) = NetworkDataPlane::poll_tx_budgeted(
            &mut network.state,
            &mut network.io,
            &mut network.ipv4_output,
            device_timestamp,
            tx_remaining,
            round_started_at,
        );
        tx_packets += dispatched;
        tx_capacity_changed |= network.io.available_data_tx_slots() > tx_capacity_before;
        has_reached_time_limit |= tx_time_limit;
        drop(network_guard);

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

        let Some(mut network_guard) = self.network.try_lock() else {
            return deferred_poll_progress(
                rx_packets,
                tx_packets,
                timer_events,
                tx_capacity_changed,
            );
        };
        let network = &mut *network_guard;
        let ingress_has_more = network.io.has_stack_ingress();
        let rx_has_more = network.io.has_rx_work(&network.state.interfaces);
        let tx_has_more = network.io.has_pending_tx();
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

    pub fn link_snapshots(&self) -> Vec<LinkSnapshot> {
        self.network.lock().state.link_snapshots()
    }

    pub fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        self.network.lock().state.link_snapshot_for_ifindex(ifindex)
    }

    pub fn has_device(&self, ifindex: i32) -> bool {
        self.network.lock().state.has_device(ifindex)
    }

    pub fn device_index_by_name(&self, name: &str) -> Option<usize> {
        self.network.lock().state.device_index_by_name(name)
    }

    pub fn link_send_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSendSnapshot> {
        self.network
            .lock()
            .state
            .link_send_snapshot_for_ifindex(ifindex)
    }

    pub(crate) fn ipv4_addr_snapshots(&self) -> Vec<crate::device::interfaces::Ipv4AddrSnapshot> {
        self.network.lock().state.ipv4_addr_snapshots()
    }

    #[cfg(unittest)]
    pub(crate) fn ipv4_addr_entries(&self) -> Vec<crate::device::interfaces::Ipv4AddrEntry> {
        self.network.lock().state.ipv4_addr_entries()
    }

    pub(crate) fn interface_ipv4_addr(
        &self,
        name: &str,
    ) -> Result<crate::ip::Ipv4Cidr, LinuxError> {
        self.network.lock().state.interface_ipv4_addr(name)
    }

    pub(crate) fn route_snapshot(&self) -> Vec<Rule> {
        self.network.lock().state.route_snapshot()
    }

    #[cfg(unittest)]
    pub(crate) fn has_neighbor(&self, dev: usize, dst: IpAddress) -> bool {
        self.network.lock().state.has_neighbor(dev, dst)
    }

    /// Prepares and sends IPv4 under one route, source, MTU, and queue transaction.
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
        let mut network = self.network.lock();
        let NetworkRuntime {
            state,
            io,
            ipv4_output,
        } = &mut *network;
        ipv4_output.prepare_and_send(
            state,
            io,
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
        self.network.lock().state.select_source_address(dst)
    }

    pub(crate) fn get_smoltcp_source_address(
        &self,
        dst: &SmoltcpIpAddress,
    ) -> KResult<SmoltcpIpAddress> {
        self.stack
            .select_source_address(&self.network.lock().state, dst)
    }

    pub(crate) fn can_send_ip_packet(&self) -> bool {
        self.network.lock().io.can_enqueue_tx_packet()
    }

    pub(crate) fn register_rx_waker(
        &self,
        mask: u32,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        let waker = self.events.register(context)?;
        self.stack.refresh_deadline(&self.events);
        let network = self.network.lock();
        network
            .io
            .register_rx_waker(&network.state.interfaces, mask, &waker, context)?;
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
        let mut network = self.network.lock();
        let NetworkRuntime { state, io, .. } = &mut *network;
        io.send_link_frame(&mut state.interfaces, ifindex, frame)
    }

    #[cfg(unittest)]
    pub(crate) fn ipv4_route_mtu(&self, dst: &IpAddress) -> Option<usize> {
        self.network.lock().state.route_mtu(dst)
    }

    #[cfg(unittest)]
    pub(crate) fn rebuild_count_for_tests(&self) -> usize {
        self.stack
            .rebuild_count
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(unittest)]
    pub(crate) fn replace_network_for_tests(&self, state: NetworkState) {
        let mut network = self.network.lock();
        *network = NetworkRuntime {
            state,
            io: NetIo::new(),
            ipv4_output: Ipv4Output::new(),
        };
        let mtu = network.state.interfaces.effective_mtu();
        self.stack.replace_for_tests(&mut network.io, mtu);
        let addrs = network.state.interfaces.local_ipv4_addrs();
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
        let _network = service.network.lock();

        let progress = service.poll_budgeted(TEST_POLL_BUDGET);

        assert_eq!(progress.rx_packets, 0);
        assert_eq!(progress.tx_packets, 0);
        assert!(progress.has_more);
    }
    #[def_test(serial)]
    fn poll_round_dispatches_tx_when_due_ipv4_expiration_lock_is_contended() {
        let loopback_addr = Ipv4Address::new(127, 0, 0, 1);
        let mut router = NetworkState::new();
        let loopback = router
            .interfaces
            .add_device(Box::new(LoopbackDevice::new()));
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
        {
            let mut guard = service.network.lock();
            let NetworkRuntime {
                state,
                io,
                ipv4_output,
            } = &mut *guard;
            ipv4_output
                .send(state, io, packet, loopback as i32 + 1)
                .unwrap();
        }
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
            let _network = service.network.lock();
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
        let network = service.network.lock();
        assert!(network.io.has_stack_ingress());
        assert!(network.io.has_pending_tx());
    }
}
