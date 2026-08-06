// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network service with budgeted data-plane progression.

use alloc::{sync::Arc, vec::Vec};
use core::{
    sync::atomic::{AtomicU64, Ordering},
    task::Waker,
};

use kerrno::{KError, KResult, LinuxError};
use khal::time::monotonic_time;
use kpoll::{PollContext, PollRegisterError, PollSet};
use ksync::Mutex;
use ktime_types::{MonotonicInstant, TimeSpan};
use smoltcp::{
    iface::{Interface, PollResult},
    time::Instant as SmoltcpInstant,
    wire::{
        HardwareAddress, IpAddress as SmoltcpIpAddress, IpCidr as SmoltcpIpCidr,
        IpListenEndpoint as SmoltcpIpListenEndpoint,
    },
};

use crate::{
    LISTEN_TABLE, SOCKET_SET,
    buf::PacketBuf,
    ip::{IpAddress, IpListenEndpoint},
    netlink::RtnetlinkState,
    poller::{BudgetKind, PollBudget, PollProgress, ROUND_TIME_CAP, TIME_CHECK_INTERVAL},
    router::Router,
    stack::ingress::IngressProcessor,
};

const IPV4_HEADER_LEN: usize = 20;
const RX_INGRESS_BATCH_PACKETS: usize = 64;

fn to_smoltcp_instant(instant: MonotonicInstant) -> SmoltcpInstant {
    let micros = instant.span_since_origin().as_micros();
    let micros = i64::try_from(micros).unwrap_or(i64::MAX);
    SmoltcpInstant::from_micros_const(micros)
}

#[cfg(unittest)]
fn from_smoltcp_instant(instant: SmoltcpInstant) -> MonotonicInstant {
    let micros = u64::try_from(instant.total_micros()).unwrap_or(0);
    MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(micros))
}

pub(crate) fn now() -> SmoltcpInstant {
    to_smoltcp_instant(monotonic_time())
}

pub struct Service {
    pub(crate) iface: Mutex<Interface>,
    router: Mutex<Router>,
    ingress: Mutex<IngressProcessor>,
    rx_batch: Mutex<Vec<PacketBuf>>,
    accepted_batch: Mutex<Vec<PacketBuf>>,
    control_batch: Mutex<Vec<Vec<u8>>>,
    timeout_deadline_micros: AtomicU64,
    timeout_poll: Arc<PollSet>,
}

impl Service {
    pub fn new(mut router: Router) -> Self {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let iface = Interface::new(config, &mut router, to_smoltcp_instant(monotonic_time()));

        Self {
            iface: Mutex::new(iface),
            router: Mutex::new(router),
            ingress: Mutex::new(IngressProcessor::new()),
            rx_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            accepted_batch: Mutex::new(Vec::with_capacity(RX_INGRESS_BATCH_PACKETS)),
            control_batch: Mutex::new(Vec::new()),
            timeout_deadline_micros: AtomicU64::new(0),
            timeout_poll: Arc::new(PollSet::new()),
        }
    }

    pub fn poll_budgeted(&self, budget: PollBudget) -> PollProgress {
        let start = monotonic_time();
        let device_timestamp = start;
        let mut overrun = None;
        let mut rx_packets = 0;
        let mut rx_remaining = budget.rx_packets;
        let mut since_time_check = 0usize;
        let mut sockets = SOCKET_SET.inner.lock();
        let mut control_packets = self.control_batch.lock();
        let mut rx_batch = self.rx_batch.lock();
        let mut accepted_packets = self.accepted_batch.lock();
        control_packets.clear();

        while rx_remaining > 0 {
            if since_time_check >= TIME_CHECK_INTERVAL {
                since_time_check = 0;
                if monotonic_time().saturating_duration_since(start) >= ROUND_TIME_CAP {
                    overrun = Some(BudgetKind::Rx);
                    break;
                }
            }

            let mut router = self.router.lock();
            // Cap each drain at TIME_CHECK_INTERVAL so the wall-time check
            // runs at least once every 16 completed RX items.
            let batch_budget = rx_remaining
                .min(RX_INGRESS_BATCH_PACKETS)
                .min(TIME_CHECK_INTERVAL)
                .min(router.ingress_capacity());
            if batch_budget == 0 {
                break;
            }
            let drain =
                router.drain_rx_budgeted_into(device_timestamp, batch_budget, &mut rx_batch);
            drop(router);
            rx_packets += drain.work_done;
            rx_remaining = rx_remaining.saturating_sub(drain.work_done);
            since_time_check = since_time_check.saturating_add(drain.work_done);

            self.ingress.lock().handle_rx_packets(
                device_timestamp,
                &mut rx_batch,
                &mut accepted_packets,
                &mut control_packets,
                &mut sockets,
            );
            let mut router = self.router.lock();
            router.enqueue_ingress_packets(&mut accepted_packets);
            for packet in control_packets.drain(..) {
                if let Err(err) = router.queue_control_ipv4_packet(packet) {
                    warn!("Dropping network control packet: {}", err);
                }
            }
            if !drain.has_more || drain.work_done == 0 {
                break;
            }
        }

        // Always run Interface::poll after RX, even when RX hit the time cap
        // or timer budget is zero. This digests ingress, advances TCP state,
        // and refreshes poll_at deadlines (T-24). Timer budget only affects
        // timer_events accounting and exhaustion feedback.
        let mut router = self.router.lock();
        let current = now();
        let mut iface = self.iface.lock();
        let iface_changed = iface.poll(current, &mut *router, &mut sockets);
        let deferred_close_deadline = sockets.reap_deferred_tcp_closes(current);
        let next_poll =
            earliest_poll_deadline(iface.poll_at(current, &sockets), deferred_close_deadline);
        drop(iface);
        let timer_more = has_immediate_timer_work(current, next_poll);
        self.update_poll_timeout(current, next_poll);
        LISTEN_TABLE.wake_touched_acceptors(&mut sockets);
        drop(sockets);

        let ingress_has_more = router.has_pending_ingress();
        let rx_has_more = router.has_immediate_rx_work();
        let rx_more = rx_has_more || ingress_has_more;
        let tx_capacity_before = router.available_tx_packet_slots();

        // iface.poll can consume most of the round budget while draining
        // smoltcp ingress injected by RX. Attribute that overrun to RX only
        // when this round actually ingested packets; a timer-only round that
        // blows the cap falls through to the Timer attribution below.
        if overrun.is_none()
            && rx_packets > 0
            && monotonic_time().saturating_duration_since(start) >= ROUND_TIME_CAP
        {
            overrun = Some(BudgetKind::Rx);
        }

        let mut tx_packets = 0;
        let mut tx_has_more = router.has_queued_tx_packets();
        let mut tx_remaining = if overrun.is_some() {
            0
        } else {
            budget.tx_packets
        };
        while tx_remaining > 0 {
            let chunk = tx_remaining.min(TIME_CHECK_INTERVAL);
            let (done, more) = router.dispatch_budgeted(device_timestamp, chunk);
            tx_packets += done;
            tx_remaining = tx_remaining.saturating_sub(done);
            tx_has_more = more;
            if done < chunk {
                break;
            }
            if monotonic_time().saturating_duration_since(start) >= ROUND_TIME_CAP {
                overrun = Some(BudgetKind::Tx);
                break;
            }
        }

        let timer_events =
            usize::from(budget.timer_events > 0 && iface_changed == PollResult::SocketStateChanged);
        let elapsed = monotonic_time().saturating_duration_since(start);
        // Timer work is a single Interface::poll. Remaining due timers count as
        // exhaustion when the timer budget slot was open this round.
        let timer_exhausted = budget.timer_events > 0 && timer_more;
        // Only attribute Timer overrun when RX/TX were idle this round and due
        // timers remain after poll; otherwise iface.poll cost belongs to RX.
        if overrun.is_none()
            && timer_exhausted
            && rx_packets == 0
            && tx_packets == 0
            && elapsed >= ROUND_TIME_CAP
        {
            overrun = Some(BudgetKind::Timer);
        }

        PollProgress {
            rx_packets,
            tx_packets,
            timer_events,
            tx_capacity_changed: router.available_tx_packet_slots() > tx_capacity_before,
            rx_exhausted: budget.rx_packets > 0 && rx_packets >= budget.rx_packets && rx_more,
            tx_exhausted: budget.tx_packets > 0 && tx_packets >= budget.tx_packets && tx_has_more,
            timer_exhausted,
            rx_more,
            tx_more: tx_has_more,
            timer_more,
            has_more: rx_more || tx_has_more || timer_more,
            elapsed,
            overrun,
        }
    }

    pub fn get_smoltcp_source_address(
        &self,
        dst_addr: &SmoltcpIpAddress,
    ) -> KResult<SmoltcpIpAddress> {
        let dst_addr = super::from_smoltcp_ip_address(*dst_addr);
        self.router
            .lock()
            .table
            .lookup(&dst_addr)
            .map(|rule| super::to_smoltcp_ip_address(rule.src))
            .ok_or(KError::from(LinuxError::ENETUNREACH))
    }

    pub fn smoltcp_device_mask_for(&self, endpoint: &SmoltcpIpListenEndpoint) -> u32 {
        match endpoint.addr {
            Some(addr) => self.smoltcp_device_mask_for_addr(&addr),
            None => u32::MAX,
        }
    }

    pub fn smoltcp_device_mask_for_addr(&self, addr: &SmoltcpIpAddress) -> u32 {
        let addr = super::from_smoltcp_ip_address(*addr);
        self.router
            .lock()
            .table
            .lookup(&addr)
            .map_or(u32::MAX, |rule| 1u32 << rule.dev)
    }

    pub fn get_source_address(&self, dst_addr: &IpAddress) -> KResult<IpAddress> {
        self.router
            .lock()
            .table
            .lookup(dst_addr)
            .map(|rule| rule.src)
            .ok_or(KError::from(LinuxError::ENETUNREACH))
    }

    pub fn can_send_ip_packet(&self) -> bool {
        self.router.lock().can_enqueue_tx_packet()
    }

    pub fn prepare_and_send_ipv4_packet(
        &self,
        bound_src: Option<IpAddress>,
        dst_addr: &IpAddress,
        packet: &mut Option<Vec<u8>>,
        prepare: impl FnOnce(&mut [u8], IpAddress, Option<usize>) -> KResult,
    ) -> KResult {
        let mut router = self.router.lock();
        let packet_len = packet.as_ref().ok_or(KError::InvalidInput)?.len();
        if !router.can_enqueue_tx_packet() {
            return Err(KError::WouldBlock);
        }

        let source_addr = match bound_src {
            Some(addr) => addr,
            None => router
                .table
                .lookup(dst_addr)
                .map(|rule| rule.src)
                .ok_or(KError::from(LinuxError::ENETUNREACH))?,
        };
        let packet_count = output_ipv4_packet_count(&router, dst_addr, packet_len)?;
        if !router.can_enqueue_tx_packets(packet_count) {
            return Err(KError::WouldBlock);
        }

        let route_mtu = router.route_mtu(dst_addr);
        prepare(
            packet.as_deref_mut().ok_or(KError::InvalidInput)?,
            source_addr,
            route_mtu,
        )?;
        router.queue_ipv4_packet(packet.take().ok_or(KError::InvalidInput)?)
    }

    pub fn device_mask_for(&self, endpoint: &IpListenEndpoint) -> u32 {
        match endpoint.addr {
            Some(addr) => self.device_mask_for_addr(&addr),
            None => u32::MAX,
        }
    }

    pub fn device_mask_for_addr(&self, addr: &IpAddress) -> u32 {
        self.router
            .lock()
            .table
            .lookup(addr)
            .map_or(u32::MAX, |rule| 1u32 << rule.dev)
    }

    pub fn register_rx_waker(
        &self,
        mask: u32,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        context.register(self.timeout_poll.as_ref())?;
        let source_waker = Waker::from(self.timeout_poll.clone());

        let current = now();
        let sockets = SOCKET_SET.inner.lock();
        let next = earliest_poll_deadline(
            self.iface.lock().poll_at(current, &sockets),
            sockets.deferred_tcp_close_deadline(),
        );
        drop(sockets);
        self.update_poll_timeout(current, next);

        for (index, device) in self.router.lock().devices.iter().enumerate() {
            if mask & (1 << index) != 0 {
                // Ethernet registers the current context directly on the IRQ
                // source. Loopback keeps the timeout_poll-backed source waker.
                device.register_rx_waker(&source_waker, context)?;
            }
        }
        Ok(source_waker)
    }

    fn update_poll_timeout(&self, current: SmoltcpInstant, next: Option<SmoltcpInstant>) {
        let Some(next) = next else {
            self.timeout_deadline_micros.store(0, Ordering::Release);
            return;
        };

        if next <= current {
            self.timeout_deadline_micros.store(0, Ordering::Release);
            self.publish_timer_expiration();
            return;
        }

        let deadline = u64::try_from(next.total_micros())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        self.timeout_deadline_micros
            .store(deadline, Ordering::Release);
    }

    pub(crate) fn handle_timer_tick(&self) {
        let deadline = self.timeout_deadline_micros.load(Ordering::Acquire);
        if deadline == 0
            || u64::try_from(now().total_micros())
                .unwrap_or(0)
                .saturating_add(1)
                < deadline
        {
            return;
        }
        if self
            .timeout_deadline_micros
            .compare_exchange(deadline, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.publish_timer_expiration();
        }
    }

    fn publish_timer_expiration(&self) {
        self.timeout_poll.as_ref().wake();
        crate::poller::network_poller().notify(crate::poller::PollReason::Timer);
    }

    pub fn sync_netlink(&self, state: &RtnetlinkState) {
        // Keep the Router locked until all derived views are refreshed so a
        // new RX batch cannot observe a partially synchronized configuration.
        let mut router = self.router.lock();
        router.sync_netlink(state);
        let local_ipv4_addrs = router.local_ipv4_addrs();

        self.ingress
            .lock()
            .update_local_ipv4_addrs(local_ipv4_addrs);
        self.iface.lock().update_ip_addrs(|ip_addrs| {
            ip_addrs.clear();
            for addr in &state.addrs {
                if let SmoltcpIpAddress::Ipv4(ipv4) = addr.address {
                    let _ = ip_addrs.push(SmoltcpIpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                        ipv4,
                        addr.prefix_len,
                    )));
                }
            }
        });
    }

    pub fn remove_device_by_model_id(&self, id: kdevice::DeviceId) -> bool {
        self.router.lock().remove_device_by_model_id(id)
    }

    pub fn send_link_frame(&self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        self.router.lock().send_link_frame(ifindex, frame)
    }
}

fn has_immediate_timer_work(current: SmoltcpInstant, next_poll: Option<SmoltcpInstant>) -> bool {
    next_poll.is_some_and(|deadline| deadline <= current)
}

fn earliest_poll_deadline(
    protocol: Option<SmoltcpInstant>,
    deferred_close: Option<SmoltcpInstant>,
) -> Option<SmoltcpInstant> {
    match (protocol, deferred_close) {
        (Some(protocol), Some(close)) => Some(protocol.min(close)),
        (protocol, close) => protocol.or(close),
    }
}

fn output_ipv4_packet_count(
    router: &Router,
    dst_addr: &IpAddress,
    packet_len: usize,
) -> KResult<usize> {
    if packet_len > u16::MAX as usize {
        return Err(LinuxError::EMSGSIZE.into());
    }

    let Some(mtu) = router.route_mtu(dst_addr) else {
        return Ok(1);
    };
    if packet_len <= mtu {
        return Ok(1);
    }

    let payload_len = packet_len
        .checked_sub(IPV4_HEADER_LEN)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    let max_fragment_payload_len = mtu
        .checked_sub(IPV4_HEADER_LEN)
        .map(|len| len / 8 * 8)
        .filter(|len| *len > 0)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?;
    Ok(payload_len
        .checked_add(max_fragment_payload_len - 1)
        .ok_or_else(|| KError::from(LinuxError::EMSGSIZE))?
        / max_fragment_payload_len)
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::*;

    #[def_test]
    fn test_smoltcp_deadline_clamps_expired_delay() {
        let current = MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(10_000));
        let deadline = from_smoltcp_instant(SmoltcpInstant::from_micros_const(9_999));

        assert_eq!(deadline.saturating_duration_since(current), TimeSpan::ZERO);
    }

    #[def_test]
    fn test_smoltcp_deadline_preserves_future_delay() {
        let current = MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(10_000));
        let deadline = from_smoltcp_instant(SmoltcpInstant::from_micros_const(12_500));

        assert_eq!(
            deadline.checked_duration_since(current),
            Some(TimeSpan::from_micros(2_500))
        );
    }

    #[def_test]
    fn future_timer_is_not_immediate_work() {
        let current = SmoltcpInstant::from_millis(10);
        let future = SmoltcpInstant::from_millis(11);

        assert!(!has_immediate_timer_work(current, Some(future)));
    }

    #[def_test]
    fn due_timer_is_immediate_work() {
        let current = SmoltcpInstant::from_millis(10);

        assert!(has_immediate_timer_work(current, Some(current)));
    }

    #[def_test]
    fn deferred_close_deadline_can_precede_protocol_deadline() {
        let close = SmoltcpInstant::from_millis(10);
        let protocol = SmoltcpInstant::from_millis(20);

        assert!(earliest_poll_deadline(Some(protocol), Some(close)) == Some(close));
    }
}
