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

use super::poll_round::{PendingWork, PollRound, PollStageResult};
use crate::{
    SOCKET_SET,
    dataplane::{DeviceRxProgress, DeviceTxProgress, NetworkDataPlane},
    io::{NetIo, RxDrain},
    ip::{IpAddress, from_smoltcp_ip_address, to_smoltcp_ip_address},
    network::{
        ipv4::{
            ingress::{Ipv4ExpirationProgress, Ipv4Ingress},
            output::{Ipv4Output, Ipv4SendOptions},
        },
        state::NetworkState,
    },
    poller::{PollBudget, PollProgress},
    stack::{
        interface::{Stack, StackProgress},
        time::{earliest_poll_deadline, has_immediate_timer_work, now},
    },
};
const POLL_ROUND_TIME_LIMIT: TimeSpan = TimeSpan::from_millis(1);
pub(crate) const POLL_TIME_CHECK_INTERVAL_WORK_ITEMS: usize = 32;

struct NetworkTimer;

impl ktask::TimerHandler for NetworkTimer {
    fn on_expiration(&self, timer: ktask::TimerControl<'_>, _: ktask::Expiration) {
        crate::SERVICE.handle_timer_tick();
        if let Err(error) = timer.rearm(monotonic_time() + crate::consts::TIMER_SAMPLE_PERIOD) {
            log::error!("[knet] timer sampling stopped: {error:?}");
        }
    }
}

pub(crate) struct Service {
    timer: ktask::Timer<NetworkTimer>,
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
    tx_capacity: Arc<PollSet>,
}
impl ServiceEvents {
    fn new() -> Self {
        Self {
            poll: Arc::new(PollSet::new()),
            tx_capacity: Arc::new(PollSet::new()),
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

    fn register_tx_capacity(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        context.register(self.tx_capacity.as_ref())?;
        Ok(Waker::from(self.tx_capacity.clone()))
    }

    fn wake_tx_capacity(&self) {
        self.tx_capacity.wake();
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
            timer: ktask::Timer::new(NetworkTimer),
            stack,
            network: state,
            io: Mutex::new(io),
            ipv4_output: Ipv4Output::new(),
            dataplane: NetworkDataPlane::new(),
            ipv4_ingress,
            events: ServiceEvents::new(),
        }
    }

    pub(crate) fn start_timer(&'static self) {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: the periodic network timer deadline \
                      must fit the initialized timer core"
        )]
        // SAFETY: the service is published after scheduler and timer setup;
        // the callback performs only IRQ-safe state transitions and wakes.
        unsafe {
            self.timer
                .start(monotonic_time() + crate::consts::TIMER_SAMPLE_PERIOD)
        }
        .expect("network sampling deadline must fit the timer core");
    }

    pub(crate) fn network(&self) -> &NetworkState {
        &self.network
    }

    pub(crate) fn select_smoltcp_source_address(
        &self,
        dst: SmoltcpIpAddress,
    ) -> KResult<SmoltcpIpAddress> {
        self.network
            .select_source_address(&from_smoltcp_ip_address(dst))
            .map(to_smoltcp_ip_address)
    }

    pub(crate) fn with_io<R>(&self, use_fn: impl FnOnce(&mut NetIo) -> R) -> R {
        use_fn(&mut self.io.lock())
    }

    pub(crate) fn try_with_io<R>(&self, use_fn: impl FnOnce(&mut NetIo) -> R) -> Option<R> {
        let mut io = self.io.try_lock()?;
        Some(use_fn(&mut io))
    }

    /// Drains RX under inventory, NetIo, then device locks, released before ingress.
    pub(crate) fn try_drain_device_rx(
        &self,
        timestamp: MonotonicInstant,
        budget: usize,
        packets: &mut Vec<crate::buf::PacketBuf>,
    ) -> Option<RxDrain> {
        self.network.try_with_interfaces(|interfaces| {
            let mut io = self.io.try_lock()?;
            let batch_budget = budget.min(io.available_stack_ingress_slots());
            Some(io.drain_device_rx_budgeted(
                interfaces.devices(),
                timestamp,
                batch_budget,
                packets,
            ))
        })?
    }

    /// Advances a bounded round in task context, retaining batches on contention.
    /// Device TX/RX and reassembly contention allow protocol progress to continue.
    pub(crate) fn poll_budgeted(&self, budget: PollBudget) -> PollProgress {
        let mut round = PollRound::new(budget, monotonic_time());
        self.poll_ipv4_timers(&mut round);
        self.flush_retained_ingress(&mut round);
        self.poll_tx(&mut round);
        self.poll_rx(&mut round);
        if self.poll_stack(&mut round) == PollStageResult::Deferred {
            let progress = round.into_deferred_progress();
            self.notify_tx_capacity_changed(&progress);
            return progress;
        }
        self.poll_remaining_tx(&mut round);
        self.poll_remaining_rx(&mut round);
        self.update_poll_deadline(&mut round);
        let progress = self.finish_poll_round(round);
        self.notify_tx_capacity_changed(&progress);
        progress
    }

    fn notify_tx_capacity_changed(&self, progress: &PollProgress) {
        if progress.tx_capacity_changed {
            self.events.wake_tx_capacity();
        }
    }

    fn poll_ipv4_timers(&self, round: &mut PollRound) {
        let progress = self.dataplane.expire_ipv4_fragments(
            &self.ipv4_ingress,
            round.started_at,
            round.budget.timer_events,
        );
        Self::apply_ipv4_timer_progress(round, progress);
    }

    fn apply_ipv4_timer_progress(round: &mut PollRound, progress: Ipv4ExpirationProgress) {
        round.add_timer_events(progress.timer_events);
        if progress.has_more {
            round.mark_pending(PendingWork::IPV4_REASSEMBLY_WORK);
        }
    }

    fn flush_retained_ingress(&self, round: &mut PollRound) {
        if !self.dataplane.flush_retained_ingress(self) {
            // Pause device RX until stack polling frees ingress capacity.
            round.mark_pending(PendingWork::RX_DEFERRED);
        }
    }

    fn poll_tx(&self, round: &mut PollRound) {
        let progress = self.dataplane.poll_tx_budgeted(
            &self.network,
            &self.io,
            &self.ipv4_output,
            round.started_at,
            round.budget.tx_packets,
            round.started_at,
        );
        Self::apply_tx_progress(round, progress);
    }

    fn apply_tx_progress(round: &mut PollRound, progress: DeviceTxProgress) {
        round.add_tx_packets(progress.packets);
        if progress.has_reached_time_limit {
            round.reach_time_limit();
        }
        if progress.tx_capacity_changed {
            round.mark_tx_capacity_changed();
        }
        if progress.has_lock_contention {
            round.mark_pending(PendingWork::TX_CONTENTION);
        }
    }

    fn poll_rx(&self, round: &mut PollRound) {
        if round.is_rx_deferred() {
            return;
        }
        let progress =
            self.dataplane
                .poll_rx_budgeted(self, round.started_at, round.remaining_rx());
        Self::apply_rx_progress(round, progress);
    }

    fn apply_rx_progress(round: &mut PollRound, progress: DeviceRxProgress) {
        round.add_rx_packets(progress.packets);
        if progress.has_reached_time_limit {
            round.reach_time_limit();
        }
        if progress.is_deferred {
            round.mark_pending(PendingWork::RX_DEFERRED);
        }
    }

    fn poll_stack(&self, round: &mut PollRound) -> PollStageResult {
        let Some(mut sockets) = SOCKET_SET.inner.try_lock() else {
            return PollStageResult::Deferred;
        };
        let Some(mut io) = self.io.try_lock() else {
            return PollStageResult::Deferred;
        };
        let Some(stack_progress) = self.stack.poll(
            &mut io,
            self.stack.device_mtu(),
            &mut sockets,
            round.budget,
            round.started_at,
            round.reached_time_limit(),
        ) else {
            return PollStageResult::Deferred;
        };
        drop(io);
        drop(sockets);
        Self::apply_stack_progress(round, stack_progress);
        PollStageResult::Complete
    }

    fn apply_stack_progress(round: &mut PollRound, progress: StackProgress) {
        round.set_reached_time_limit(progress.has_reached_time_limit);
        if progress.has_socket_state_change && round.can_record_timer_event() {
            round.add_timer_events(1);
        }
        round.set_next_poll_deadline(earliest_poll_deadline(
            progress.protocol_deadline,
            progress.deferred_close_deadline,
        ));
    }

    fn poll_remaining_tx(&self, round: &mut PollRound) {
        let remaining = round.remaining_tx();
        if remaining == 0 {
            return;
        }
        let progress = self.dataplane.poll_tx_budgeted(
            &self.network,
            &self.io,
            &self.ipv4_output,
            round.started_at,
            remaining,
            round.started_at,
        );
        Self::apply_tx_progress(round, progress);
    }

    fn poll_remaining_rx(&self, round: &mut PollRound) {
        let remaining = round.remaining_rx();
        if remaining == 0 {
            return;
        }
        let progress = self
            .dataplane
            .poll_rx_budgeted(self, round.started_at, remaining);
        Self::apply_rx_progress(round, progress);
    }

    fn update_poll_deadline(&self, round: &mut PollRound) {
        let next_poll = round.next_poll_deadline();
        let timer_current = now();
        if has_immediate_timer_work(timer_current, next_poll) {
            round.mark_pending(PendingWork::PROTOCOL_TIMER);
        }
        self.stack
            .update_protocol_deadline(timer_current, next_poll, &self.events);
    }

    fn finish_poll_round(&self, mut round: PollRound) -> PollProgress {
        if let Some(io) = self.io.try_lock() {
            if io.has_stack_ingress() {
                round.mark_pending(PendingWork::INGRESS_QUEUE);
            }
            if io.has_pending_tx() {
                round.mark_pending(PendingWork::TX_QUEUE);
            }
        } else {
            round.mark_pending(PendingWork::QUEUE_SNAPSHOT_CONTENTION);
        }
        match self
            .network
            .try_with_interfaces(|interfaces| interfaces.has_rx_work())
        {
            Some(true) => round.mark_pending(PendingWork::DEVICE_RX_READY),
            Some(false) => {}
            None => round.mark_pending(PendingWork::INTERFACE_SNAPSHOT_CONTENTION),
        }
        if self.dataplane.has_pending_tx() {
            round.mark_pending(PendingWork::DATAPLANE_TX);
        }
        if self.ipv4_ingress.has_pending_timer() {
            round.mark_pending(PendingWork::IPV4_REASSEMBLY_TIMER);
        }
        round.into_progress()
    }

    /// Resolves the route before the queue preparation and enqueue transaction.
    /// Capacity failure preserves the caller's packet for retry.
    pub(crate) fn prepare_and_send_ipv4_packet(
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

    pub(crate) fn register_tx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        self.events.register_tx_capacity(context)
    }

    pub(crate) fn handle_timer_tick(&self) {
        let current_micros = u64::try_from(now().total_micros()).unwrap_or(0);
        let protocol_expired = self.stack.handle_timer_tick(current_micros);
        let reassembly_expired = self.ipv4_ingress.handle_timer_tick(current_micros);
        if protocol_expired || reassembly_expired {
            self.events.notify_timer();
        }
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
                io.on_device_removed(interfaces.devices().len(), removed_index);
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
    #[def_test(serial)]
    fn poll_round_continues_stack_after_tx_and_rx_lock_contention() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicUsize;

        use crate::{device::test_device::TestDevice, router::ROUTE_SCOPE_UNIVERSE};

        let sends = Arc::new(AtomicUsize::new(0));
        let state = NetworkState::new();
        let src = Ipv4Address::new(192, 0, 2, 1);
        let dev = state.add_device(Box::new(TestDevice::ethernet(1, 1500).with_send_hook({
            let sends = sends.clone();
            move || {
                sends.fetch_add(1, Ordering::Relaxed);
            }
        })));
        state
            .add_ipv4_addr(Ipv4AddrEntry {
                dev,
                addr: Ipv4Cidr::new(src, 24).unwrap(),
                scope: ROUTE_SCOPE_UNIVERSE,
                broadcast: None,
            })
            .unwrap();
        let packet = ipv4::build_ipv4_packet(
            src,
            Ipv4Address::new(192, 0, 2, 2),
            ipv4::PROTOCOL_ICMP,
            64,
            &[],
        )
        .unwrap();
        let service = Service::new(state);
        let dst = IpAddress::Ipv4(Ipv4Address::new(192, 0, 2, 2));
        let mtu = service.network.route_mtu(&dst);
        service
            .with_io(|io| service.ipv4_output.send(io, mtu, packet, 0))
            .unwrap();

        const SENTINEL: u64 = 0xDEAD;
        service
            .stack
            .protocol_deadline_micros
            .store(SENTINEL, Ordering::Release);
        let _interfaces = service.network.try_write_interfaces().unwrap();

        let progress = service.poll_budgeted(TEST_POLL_BUDGET);

        assert_eq!(progress.tx_packets, 0);
        assert_eq!(progress.rx_packets, 0);
        assert_eq!(sends.load(Ordering::Relaxed), 0);
        assert!(progress.has_more);
        assert!(service.dataplane.has_pending_tx());
        assert!(
            service
                .stack
                .protocol_deadline_micros
                .load(Ordering::Acquire)
                != SENTINEL
        );
    }

    #[def_test(serial)]
    fn poll_round_continues_protocol_on_device_mutex_contention() {
        use crate::device::test_device::TestDevice;

        let state = NetworkState::new();
        state.add_device(Box::new(TestDevice::ethernet(1, 1500)));
        let handle = state.with_interfaces(|interfaces| interfaces.devices()[0].clone());
        let service = Service::new(state);
        const SENTINEL: u64 = 0xDEAD;
        service
            .stack
            .protocol_deadline_micros
            .store(SENTINEL, Ordering::Release);

        let _device = handle.lock();
        let progress = service.poll_budgeted(TEST_POLL_BUDGET);

        assert_eq!(progress.rx_packets, 0);
        assert!(progress.has_more);
        assert!(
            service
                .stack
                .protocol_deadline_micros
                .load(Ordering::Acquire)
                != SENTINEL
        );
        assert!(service.io.try_lock().is_some());
    }

    #[def_test(serial)]
    fn poll_round_dispatches_tx_when_due_ipv4_expiration_lock_is_contended() {
        let loopback_addr = Ipv4Address::new(127, 0, 0, 1);
        let router = NetworkState::new();
        let loopback = router.add_device(Box::new(LoopbackDevice::new()));
        router
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

    #[def_test(serial)]
    fn ingress_handoff_defers_listener_preparation_until_batch_fits() {
        use alloc::vec;

        use smoltcp::wire::{IpAddress as SmoltcpIpAddress, IpListenEndpoint};

        use crate::{
            LISTEN_TABLE,
            buf::{PacketBuf, PacketOwner},
        };

        struct ListenerGuard(IpListenEndpoint);
        impl Drop for ListenerGuard {
            fn drop(&mut self) {
                LISTEN_TABLE.unlisten(self.0);
            }
        }

        let src = Ipv4Address::new(192, 0, 2, 10);
        let dst = Ipv4Address::new(192, 0, 2, 11);
        let listener = (49152..=65535)
            .find_map(|port| {
                let endpoint = IpListenEndpoint {
                    addr: Some(SmoltcpIpAddress::Ipv4(dst.into())),
                    port,
                };
                LISTEN_TABLE
                    .listen(endpoint, 1)
                    .ok()
                    .map(|()| ListenerGuard(endpoint))
            })
            .unwrap();
        let socket_count = SOCKET_SET.inner.lock().iter().count();
        let mut syn = [0u8; 20];
        syn[..2].copy_from_slice(&12345u16.to_be_bytes());
        syn[2..4].copy_from_slice(&listener.0.port.to_be_bytes());
        syn[12] = 5 << 4;
        syn[13] = 0x02;
        let syn = PacketBuf::from_ip_packet_vec(
            1,
            ipv4::build_ipv4_packet(src, dst, ipv4::PROTOCOL_TCP, 64, &syn).unwrap(),
            PacketOwner::DeviceRx,
        );
        let mut accepted = vec![syn.clone(), syn.clone()];
        let mut control = vec![vec![0; 20]];
        let service = Service::new(NetworkState::new());
        service.with_io(|io| {
            let mut queued = vec![
                PacketBuf::from_ip_packet_vec(0, vec![0], PacketOwner::DeviceRx);
                io.available_stack_ingress_slots() - 1
            ];
            io.enqueue_stack_ingress(&mut queued).unwrap();
        });

        assert!(
            !service
                .dataplane
                .flush_ingress(&service, &mut accepted, &mut control)
        );
        assert_eq!(SOCKET_SET.inner.lock().iter().count(), socket_count);
        assert_eq!(accepted.len(), 2);
        assert!(
            accepted
                .iter()
                .all(|packet| packet.shares_storage_with(&syn))
        );
        assert_eq!(control, vec![vec![0; 20]]);
        assert_eq!(service.with_io(|io| io.available_stack_ingress_slots()), 1);
        assert!(!service.with_io(|io| io.has_pending_tx()));

        service.with_io(|io| io.pop_stack_ingress()).unwrap();
        assert!(
            service
                .dataplane
                .flush_ingress(&service, &mut accepted, &mut control)
        );
        assert_eq!(SOCKET_SET.inner.lock().iter().count(), socket_count + 1);
        assert!(accepted.is_empty());
        assert!(control.is_empty());
        assert_eq!(service.with_io(|io| io.available_stack_ingress_slots()), 0);
        assert!(service.with_io(|io| io.has_pending_tx()));
    }
}
