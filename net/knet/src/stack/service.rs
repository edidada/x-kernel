// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network service with budgeted data-plane progression.

use alloc::{boxed::Box, sync::Arc, task::Wake, vec::Vec};
use core::{
    pin::Pin,
    task::{Context, Waker},
};

use kerrno::{KError, KResult, LinuxError};
use khal::time::monotonic_time;
use kpoll::{PollContext, PollRegisterError, PollSet};
use ksync::Mutex;
use ktask::future::sleep_until;
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
    poller::{PollBudget, PollProgress},
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

fn from_smoltcp_instant(instant: SmoltcpInstant) -> MonotonicInstant {
    let micros = u64::try_from(instant.total_micros()).unwrap_or(0);
    MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(micros))
}

struct TimeoutState {
    future: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    deadline: Option<MonotonicInstant>,
}

struct TimeoutWake {
    socket_waiters: Arc<PollSet>,
}

impl Wake for TimeoutWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.socket_waiters.as_ref().wake();
        crate::poller::network_poller().schedule(crate::poller::PollReason::Timer);
    }
}

pub struct Service {
    pub(crate) iface: Mutex<Interface>,
    router: Mutex<Router>,
    ingress: Mutex<IngressProcessor>,
    rx_batch: Mutex<Vec<PacketBuf>>,
    accepted_batch: Mutex<Vec<PacketBuf>>,
    control_batch: Mutex<Vec<Vec<u8>>>,
    timeout: Mutex<TimeoutState>,
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
            timeout: Mutex::new(TimeoutState {
                future: None,
                deadline: None,
            }),
            timeout_poll: Arc::new(PollSet::new()),
        }
    }

    pub fn poll_budgeted(&self, budget: PollBudget) -> PollProgress {
        let device_timestamp = monotonic_time();
        let mut rx_packets = 0;
        let mut rx_has_more = false;
        let mut rx_remaining = budget.rx_packets;
        let mut sockets = SOCKET_SET.inner.lock();
        let mut control_packets = self.control_batch.lock();
        let mut rx_batch = self.rx_batch.lock();
        let mut accepted_packets = self.accepted_batch.lock();
        control_packets.clear();

        while rx_remaining > 0 {
            let mut router = self.router.lock();
            let batch_budget = rx_remaining
                .min(RX_INGRESS_BATCH_PACKETS)
                .min(router.ingress_capacity());
            if batch_budget == 0 {
                rx_has_more = true;
                break;
            }
            let drain =
                router.drain_rx_budgeted_into(device_timestamp, batch_budget, &mut rx_batch);
            drop(router);
            rx_packets += drain.work_done;
            rx_remaining = rx_remaining.saturating_sub(drain.work_done);

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
            rx_has_more = drain.has_more;
            if !drain.has_more || drain.work_done == 0 {
                break;
            }
        }

        let mut router = self.router.lock();
        let current = to_smoltcp_instant(device_timestamp);
        let mut iface = self.iface.lock();
        let iface_changed = iface.poll(current, &mut *router, &mut sockets);
        let next_poll = iface.poll_at(current, &sockets);
        drop(iface);
        self.update_poll_timeout(device_timestamp, next_poll);
        LISTEN_TABLE.wake_touched_acceptors(&mut sockets);
        drop(sockets);

        let ingress_has_more = router.has_pending_ingress();
        let (tx_packets, tx_has_more) =
            router.dispatch_budgeted(device_timestamp, budget.tx_packets);
        PollProgress {
            rx_packets,
            tx_packets,
            timer_events: usize::from(
                budget.timer_events > 0 && iface_changed == PollResult::SocketStateChanged,
            ),
            has_more: rx_has_more || ingress_has_more || tx_has_more,
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

        let current = monotonic_time();
        let sockets = SOCKET_SET.inner.lock();
        let next = self
            .iface
            .lock()
            .poll_at(to_smoltcp_instant(current), &sockets);
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

    fn update_poll_timeout(&self, current: MonotonicInstant, next: Option<SmoltcpInstant>) {
        let mut timeout = self.timeout.lock();
        if let Some(next) = next {
            let deadline = from_smoltcp_instant(next);
            let should_reset = match timeout.deadline {
                None => true,
                Some(old_deadline) => current >= old_deadline || deadline < old_deadline,
            };

            if should_reset {
                timeout.future = None;
                timeout.deadline = Some(deadline);
                let mut future = Box::pin(sleep_until(deadline));
                let wake = Waker::from(Arc::new(TimeoutWake {
                    socket_waiters: self.timeout_poll.clone(),
                }));
                let mut context = Context::from_waker(&wake);
                if future.as_mut().poll(&mut context).is_ready() {
                    timeout.deadline = None;
                    self.timeout_poll.as_ref().wake();
                    crate::poller::network_poller().schedule(crate::poller::PollReason::Timer);
                } else {
                    timeout.future = Some(future);
                }
            }
        } else {
            timeout.future = None;
            timeout.deadline = None;
        }
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
    use unittest::def_test;

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
}
