// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network service wrapper around smoltcp interface.
use alloc::{boxed::Box, sync::Arc};
use core::{
    pin::Pin,
    task::{Context, Waker},
};

use kerrno::{KError, KResult, LinuxError};
use khal::time::monotonic_time;
use kpoll::{PollContext, PollRegisterError, PollSet};
use ktask::future::sleep;
use ktime_types::{MonotonicInstant, TimeSpan};
use smoltcp::{
    iface::{Interface, SocketSet},
    time::Instant as SmoltcpInstant,
    wire::{
        HardwareAddress, IpAddress as SmoltcpIpAddress, IpCidr as SmoltcpIpCidr,
        IpListenEndpoint as SmoltcpIpListenEndpoint,
    },
};

use crate::{
    LISTEN_TABLE, SOCKET_SET,
    device::{LinkConfigUpdate, LinkSnapshot, NeighborUpdate},
    ip::{IpAddress, IpListenEndpoint},
    router::{Ipv4AddrEntry, Ipv4AddrSnapshot, Router, Rule},
};

const IPV4_HEADER_LEN: usize = 20;

fn to_smoltcp_instant(instant: MonotonicInstant) -> SmoltcpInstant {
    let micros = instant.span_since_origin().as_micros();
    let micros = i64::try_from(micros).unwrap_or(i64::MAX);
    SmoltcpInstant::from_micros_const(micros)
}

#[cfg_attr(not(unittest), allow(dead_code))]
fn from_smoltcp_instant(instant: SmoltcpInstant) -> MonotonicInstant {
    let micros = u64::try_from(instant.total_micros()).unwrap_or(0);
    MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(micros))
}

pub struct Service {
    pub iface: Interface,
    router: Router,
    timeout: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    timeout_deadline: Option<MonotonicInstant>,
    timeout_poll: Arc<PollSet>,
}
impl Service {
    pub fn new(mut router: Router) -> Self {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let iface = Interface::new(config, &mut router, to_smoltcp_instant(monotonic_time()));

        let mut service = Self {
            iface,
            router,
            timeout: None,
            timeout_deadline: None,
            timeout_poll: Arc::new(PollSet::new()),
        };
        service.sync_interface_ipv4_addrs();
        service
    }

    pub fn poll(&mut self, sockets: &mut SocketSet) -> bool {
        let current = monotonic_time();
        let smoltcp_timestamp = to_smoltcp_instant(current);

        self.router.poll(current, sockets);
        self.iface
            .poll(smoltcp_timestamp, &mut self.router, sockets);
        LISTEN_TABLE.wake_touched_acceptors(sockets);
        // Keep timeout arming on the wait path (`register_rx_waker`) only, and
        // only for smoltcp interface timers. Device deadlines (ARP/pending TX)
        // are driven by `knet-rx`, not by waking every socket waiter.
        self.router.dispatch(current)
    }

    pub fn get_smoltcp_source_address(
        &self,
        dst_addr: &SmoltcpIpAddress,
    ) -> KResult<SmoltcpIpAddress> {
        let dst_addr = super::from_smoltcp_ip_address(*dst_addr);
        self.router
            .source_address_for(&dst_addr)
            .map(super::to_smoltcp_ip_address)
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
            .table
            .lookup(&addr)
            .map_or(u32::MAX, |it| 1u32 << it.dev)
    }

    pub fn get_source_address(&self, dst_addr: &IpAddress) -> KResult<IpAddress> {
        self.router
            .source_address_for(dst_addr)
            .ok_or(KError::from(LinuxError::ENETUNREACH))
    }

    pub fn can_send_ip_packet(&self) -> bool {
        self.router.can_enqueue_tx_packet()
    }

    pub fn prepare_ipv4_packet_send(
        &self,
        bound_src: Option<IpAddress>,
        dst_addr: &IpAddress,
        packet_len: usize,
    ) -> KResult<IpAddress> {
        if !self.router.can_enqueue_tx_packet() {
            return Err(KError::WouldBlock);
        }

        let source_addr = match bound_src {
            Some(addr) if self.router.has_ipv4_address(addr) => addr,
            Some(_) => return Err(LinuxError::ENETUNREACH.into()),
            None => self
                .router
                .source_address_for(dst_addr)
                .ok_or(KError::from(LinuxError::ENETUNREACH))?,
        };
        let packet_count = output_ipv4_packet_count(&self.router, dst_addr, packet_len)?;
        if !self.router.can_enqueue_tx_packets(packet_count) {
            return Err(KError::WouldBlock);
        }

        Ok(source_addr)
    }

    pub fn send_ipv4_packet(&mut self, packet: alloc::vec::Vec<u8>) -> KResult {
        self.router.queue_ipv4_packet(packet)
    }

    pub fn ipv4_route_mtu(&self, dst_addr: &IpAddress) -> Option<usize> {
        self.router.route_mtu(dst_addr)
    }

    pub fn device_mask_for(&self, endpoint: &IpListenEndpoint) -> u32 {
        match endpoint.addr {
            Some(addr) => self.device_mask_for_addr(&addr),
            None => u32::MAX,
        }
    }

    pub fn device_mask_for_addr(&self, addr: &IpAddress) -> u32 {
        self.router
            .table
            .lookup(addr)
            .map_or(u32::MAX, |rule| 1u32 << rule.dev)
    }

    pub fn register_rx_waker(
        &mut self,
        mask: u32,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        context.register(&self.timeout_poll)?;
        let source_waker = Waker::from(self.timeout_poll.clone());

        self.schedule_next_poll(&SOCKET_SET.inner.lock());

        for (i, device) in self.router.devices.iter().enumerate() {
            if mask & (1 << i) != 0 {
                // Ethernet registers context directly on the IRQ source.
                // Loopback keeps the timeout_poll-backed source waker for fan-out.
                device.register_rx_waker(&source_waker, context)?;
            }
        }
        Ok(source_waker)
    }

    /// Earliest device-maintained poll deadline (ARP / pending TX), if any.
    ///
    /// Driven by `knet-rx`, not by socket `register_rx_waker`: merging this into
    /// the shared `timeout_poll` path wakes every waiter whenever a pending
    /// packet uses `MonotonicInstant::ORIGIN`, which livelocks concurrent loopback TCP
    /// under slow TCG.
    pub(crate) fn device_next_poll_at(&self) -> Option<MonotonicInstant> {
        self.router.next_poll_at()
    }

    fn schedule_next_poll(&mut self, sockets: &SocketSet<'_>) {
        let smoltcp_now = to_smoltcp_instant(monotonic_time());
        let monotonic_now = monotonic_time();
        // Socket waiters only arm smoltcp interface timers (same as main).
        // Device deadlines are owned by `knet-rx`.
        let deadline = self.iface.poll_at(smoltcp_now, sockets).map(|next| {
            let delay_micros = next
                .total_micros()
                .saturating_sub(smoltcp_now.total_micros()) as u64;
            monotonic_now + TimeSpan::from_micros(delay_micros)
        });

        if let Some(deadline) = deadline {
            let should_reset = match self.timeout_deadline {
                None => true,
                Some(old_deadline) => monotonic_now >= old_deadline || deadline < old_deadline,
            };

            if should_reset {
                self.timeout = None;
                self.timeout_deadline = Some(deadline);

                let delay = deadline.saturating_duration_since(monotonic_now);
                let mut fut = Box::pin(sleep(delay));
                let wake = Waker::from(self.timeout_poll.clone());
                let mut cx = Context::from_waker(&wake);

                if fut.as_mut().poll(&mut cx).is_ready() {
                    self.timeout_deadline = None;
                    self.timeout_poll.wake();
                } else {
                    self.timeout = Some(fut);
                }
            }
        } else {
            self.timeout = None;
            self.timeout_deadline = None;
        }
    }

    fn notify_poll_deadline_changed(&mut self) {
        self.timeout = None;
        self.timeout_deadline = None;
        self.timeout_poll.wake();
    }

    fn sync_interface_ipv4_addrs(&mut self) {
        let local_ipv4_addrs = self.router.local_ipv4_addrs();
        Self::replace_interface_ipv4_addrs(&mut self.iface, &local_ipv4_addrs);
    }

    fn replace_interface_ipv4_addrs(iface: &mut Interface, addrs: &[crate::ip::Ipv4Cidr]) {
        debug_assert!(addrs.len() <= smoltcp::config::IFACE_MAX_ADDR_COUNT);
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs.clear();
            for addr in addrs {
                ip_addrs
                    .push(SmoltcpIpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                        addr.address().into(),
                        addr.prefix_len(),
                    )))
                    .expect("Router validates the smoltcp address capacity");
            }
        });
    }

    fn rebuild_interface(&mut self) {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let mut iface = Interface::new(
            config,
            &mut self.router,
            to_smoltcp_instant(monotonic_time()),
        );
        let local_ipv4_addrs = self.router.local_ipv4_addrs();
        Self::replace_interface_ipv4_addrs(&mut iface, &local_ipv4_addrs);
        self.iface = iface;
        self.notify_poll_deadline_changed();
    }

    pub fn link_snapshots(&self) -> alloc::vec::Vec<LinkSnapshot> {
        self.router.link_snapshots()
    }

    pub fn ipv4_addr_snapshots(&self) -> alloc::vec::Vec<Ipv4AddrSnapshot> {
        self.router.ipv4_addr_snapshots()
    }

    pub fn route_snapshot(&self) -> alloc::vec::Vec<Rule> {
        self.router.route_snapshot()
    }

    pub fn link_snapshot_for_ifindex(&self, ifindex: i32) -> Option<LinkSnapshot> {
        self.router.link_snapshot_for_ifindex(ifindex)
    }

    pub fn has_device(&self, dev: usize) -> bool {
        self.router.has_device(dev)
    }

    pub fn ipv4_addr_entries(&self) -> &[Ipv4AddrEntry] {
        self.router.ipv4_addr_entries()
    }

    pub fn has_ipv4_address(&self, addr: crate::ip::IpAddress) -> bool {
        self.router.has_ipv4_address(addr)
    }

    pub fn is_ipv4_broadcast(&self, addr: crate::ip::IpAddress) -> bool {
        self.router.is_ipv4_broadcast(addr)
    }

    pub fn is_ipv4_bindable_broadcast(&self, addr: crate::ip::IpAddress) -> bool {
        self.router.is_ipv4_bindable_broadcast(addr)
    }

    pub fn route_rules(&self) -> &[Rule] {
        self.router.route_rules()
    }

    pub fn has_neighbor(&self, dev: usize, dst: crate::ip::IpAddress) -> bool {
        self.router.has_neighbor(dev, dst)
    }

    pub fn update_device_link(
        &mut self,
        dev: usize,
        update: LinkConfigUpdate,
    ) -> Result<(), LinuxError> {
        let mtu_changed = update.mtu.is_some();
        let link_state_changed = update.is_up.is_some();
        self.router.update_device_link(dev, update)?;
        if mtu_changed {
            self.rebuild_interface();
        } else if link_state_changed {
            self.notify_poll_deadline_changed();
        }
        Ok(())
    }

    pub fn add_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        if self
            .router
            .link_snapshot_for_ifindex(entry.dev as i32 + 1)
            .is_none()
        {
            return Err(LinuxError::ENODEV);
        }
        self.router.add_ipv4_addr(entry)?;
        self.sync_interface_ipv4_addrs();
        self.notify_poll_deadline_changed();
        Ok(())
    }

    pub fn remove_ipv4_addr(&mut self, entry: Ipv4AddrEntry) -> Result<(), LinuxError> {
        if !self.router.remove_ipv4_addr(entry) {
            return Err(LinuxError::EADDRNOTAVAIL);
        }
        self.sync_interface_ipv4_addrs();
        self.notify_poll_deadline_changed();
        Ok(())
    }

    pub fn add_route_rule(&mut self, rule: Rule) -> Result<(), LinuxError> {
        self.router.add_route_rule(rule)
    }

    pub fn replace_route_rule(
        &mut self,
        existing: Rule,
        replacement: Rule,
    ) -> Result<(), LinuxError> {
        self.router.replace_route_rule(existing, replacement)
    }

    pub fn remove_route_rule(&mut self, route: Rule) -> Result<(), LinuxError> {
        self.router.remove_exact_route_rule(route);
        Ok(())
    }

    pub fn apply_neighbor_update(&mut self, update: NeighborUpdate) -> Result<(), LinuxError> {
        if self
            .router
            .link_snapshot_for_ifindex(update.dev as i32 + 1)
            .is_none()
        {
            return Err(LinuxError::ENODEV);
        }
        self.router.apply_neighbor_update(update)?;
        self.notify_poll_deadline_changed();
        Ok(())
    }

    #[cfg(unittest)]
    pub(crate) fn replace_router_for_tests(&mut self, router: Router) {
        self.router = router;
        self.rebuild_interface();
    }

    pub fn remove_device_by_model_id(&mut self, id: kdevice::DeviceId) -> bool {
        let removed = self.router.remove_device_by_model_id(id);
        if removed {
            self.rebuild_interface();
        }
        removed
    }

    pub fn send_link_frame(&mut self, ifindex: i32, frame: &[u8]) -> KResult<usize> {
        self.router.send_link_frame(ifindex, frame)
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

    let mtu = router.egress_mtu_for(dst_addr)?;
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
