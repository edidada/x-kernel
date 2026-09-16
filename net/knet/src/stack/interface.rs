// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! smoltcp interface, staged progression, and protocol timer ownership.
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use khal::time::monotonic_time;
use ksync::Mutex;
use ktime_types::MonotonicInstant;
use smoltcp::{
    iface::{Interface, PollIngressSingleResult, PollResult, SocketSet},
    time::Instant as SmoltcpInstant,
    wire::{HardwareAddress, IpCidr as SmoltcpIpCidr},
};

use crate::{
    LISTEN_TABLE, SOCKET_SET,
    buf::PacketBuf,
    io::NetIo,
    ip,
    poller::PollBudget,
    service::{
        POLL_TIME_CHECK_INTERVAL_WORK_ITEMS, ServiceEvents, has_reached_poll_round_time_limit,
    },
    stack::{
        device::StackDevice,
        time::{earliest_poll_deadline, now, to_smoltcp_instant},
    },
};
#[derive(Default)]
pub(crate) struct SmoltcpIngressProgress {
    pub packets: usize,
    pub has_socket_state_change: bool,
    pub has_reached_time_limit: bool,
}
pub(crate) struct StackProgress {
    pub has_reached_time_limit: bool,
    pub has_socket_state_change: bool,
    pub protocol_deadline: Option<SmoltcpInstant>,
    pub deferred_close_deadline: Option<SmoltcpInstant>,
}
pub(crate) struct Stack {
    iface: Mutex<Interface>,
    /// Aggregate smoltcp MTU; egress uses [`crate::network::state::ResolvedRoute::mtu`].
    mtu: AtomicUsize,
    pub(crate) protocol_deadline_micros: AtomicU64,
    #[cfg(unittest)]
    pub(crate) rebuild_count: AtomicUsize,
}
impl Stack {
    pub(crate) fn new(io: &mut NetIo, mtu: usize) -> Self {
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let iface = Interface::new(config, &mut StackDevice::new(io, mtu), now());
        Self {
            iface: Mutex::new(iface),
            mtu: AtomicUsize::new(mtu),
            protocol_deadline_micros: AtomicU64::new(0),
            #[cfg(unittest)]
            rebuild_count: AtomicUsize::new(0),
        }
    }

    pub(crate) fn device_mtu(&self) -> usize {
        self.mtu.load(Ordering::Acquire)
    }

    pub(crate) fn set_ipv4_addrs(&self, addrs: &[ip::Ipv4Cidr]) {
        self.iface.lock().update_ip_addrs(|ip_addrs| {
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

    /// Provides the interface context while the caller holds the socket set lock.
    pub(crate) fn with_context<R>(
        &self,
        use_fn: impl FnOnce(&mut smoltcp::iface::Context) -> R,
    ) -> R {
        use_fn(self.iface.lock().context())
    }

    pub(crate) fn prepare_ingress(packets: &[PacketBuf], sockets: &mut SocketSet<'_>) {
        super::ingress::prepare_smoltcp_ingress(packets, sockets);
    }

    pub(crate) fn refresh_deadline(&self, events: &ServiceEvents) {
        let current = now();
        let sockets = SOCKET_SET.inner.lock();
        let next = earliest_poll_deadline(
            Self::next_poll_deadline(&mut self.iface.lock(), current, &sockets),
            sockets.deferred_tcp_close_deadline(),
        );
        drop(sockets);
        self.update_protocol_deadline(current, next, events);
    }

    pub(crate) fn handle_timer_tick(&self, current_micros: u64) -> bool {
        let protocol_deadline = self.protocol_deadline_micros.load(Ordering::Acquire);
        protocol_deadline != 0
            && current_micros.saturating_add(1) >= protocol_deadline
            && self
                .protocol_deadline_micros
                .compare_exchange(protocol_deadline, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    /// Advances smoltcp with the socket set and `NetIo` already held.
    pub(crate) fn poll(
        &self,
        io: &mut NetIo,
        mtu: usize,
        sockets: &mut crate::stack::socket_set::SocketSetState,
        budget: PollBudget,
        round_started_at: MonotonicInstant,
        mut has_reached_time_limit: bool,
    ) -> Option<StackProgress> {
        let current = now();
        let mut iface = self.iface.try_lock()?;
        let mut device = StackDevice::new(io, mtu);
        // Maintenance and one egress pass remain bounded and preserve protocol
        // progress after bulk RX or TX reaches the round time limit.
        Self::poll_maintenance(&mut iface, current);

        let ingress_progress = if has_reached_time_limit {
            SmoltcpIngressProgress::default()
        } else {
            Self::poll_ingress_budgeted(
                &mut iface,
                current,
                &mut device,
                sockets,
                budget.rx_packets,
                round_started_at,
            )
        };
        has_reached_time_limit |= ingress_progress.has_reached_time_limit;
        let mut has_socket_state_change = ingress_progress.has_socket_state_change;

        let (egress_changed, egress_time_limit) = Self::poll_egress_budgeted(
            &mut iface,
            current,
            &mut device,
            sockets,
            budget.tx_packets.max(1),
            round_started_at,
            has_reached_time_limit,
        );
        has_socket_state_change |= egress_changed;
        has_reached_time_limit = egress_time_limit;

        let deferred_close_deadline = sockets.reap_deferred_tcp_closes(current);
        let protocol_deadline = Self::next_poll_deadline(&mut iface, current, sockets);
        drop(iface);
        Self::finish_poll(sockets);
        Some(StackProgress {
            has_reached_time_limit,
            has_socket_state_change,
            protocol_deadline,
            deferred_close_deadline,
        })
    }

    fn poll_maintenance(iface: &mut Interface, current: SmoltcpInstant) {
        iface.poll_maintenance(current);
    }

    fn next_poll_deadline(
        iface: &mut Interface,
        current: SmoltcpInstant,
        sockets: &SocketSet<'_>,
    ) -> Option<SmoltcpInstant> {
        iface.poll_at(current, sockets)
    }

    fn finish_poll(sockets: &mut SocketSet<'_>) {
        LISTEN_TABLE.refresh_acceptors(sockets);
    }

    /// Returns socket-state and time-limit progress, preserving one egress pass after the soft limit.
    fn poll_egress_budgeted(
        iface: &mut Interface,
        current: SmoltcpInstant,
        device: &mut StackDevice<'_>,
        sockets: &mut SocketSet<'_>,
        pass_limit: usize,
        round_started_at: MonotonicInstant,
        mut has_reached_time_limit: bool,
    ) -> (bool, bool) {
        let mut has_socket_state_change = false;
        let mut stack_tx_passes = 0;
        let stack_tx_pass_limit = pass_limit;
        loop {
            let result = iface.poll_egress(current, device, sockets);
            if result == PollResult::None {
                break;
            }
            has_socket_state_change = true;
            stack_tx_passes += 1;
            if stack_tx_passes >= stack_tx_pass_limit || has_reached_time_limit {
                break;
            }
            if stack_tx_passes.is_multiple_of(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS)
                && has_reached_poll_round_time_limit(round_started_at, monotonic_time())
            {
                has_reached_time_limit = true;
                break;
            }
        }
        if !has_reached_time_limit
            && stack_tx_passes > 0
            && has_reached_poll_round_time_limit(round_started_at, monotonic_time())
        {
            has_reached_time_limit = true;
        }

        (has_socket_state_change, has_reached_time_limit)
    }

    pub(crate) fn update_protocol_deadline(
        &self,
        current: SmoltcpInstant,
        next: Option<SmoltcpInstant>,
        events: &ServiceEvents,
    ) {
        let Some(next) = next else {
            self.protocol_deadline_micros.store(0, Ordering::Release);
            return;
        };

        if next <= current {
            self.protocol_deadline_micros.store(0, Ordering::Release);
            events.notify_timer();
            return;
        }

        let deadline = u64::try_from(next.total_micros())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        self.protocol_deadline_micros
            .store(deadline, Ordering::Release);
    }

    pub(crate) fn rebuild_interface(&self, io: &mut NetIo, mtu: usize, events: &ServiceEvents) {
        let mut iface = self.iface.lock();
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let mut new_iface = Interface::new(
            config,
            &mut StackDevice::new(io, mtu),
            to_smoltcp_instant(monotonic_time()),
        );
        let current_addrs = iface.ip_addrs().to_vec();
        new_iface.update_ip_addrs(|ip_addrs| {
            for addr in current_addrs {
                ip_addrs
                    .push(addr)
                    .expect("rebuilt interface must preserve the original address capacity");
            }
        });
        *iface = new_iface;
        self.mtu.store(mtu, Ordering::Release);
        self.protocol_deadline_micros.store(0, Ordering::Release);
        events.wake();
        #[cfg(unittest)]
        self.rebuild_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn poll_ingress_budgeted(
        iface: &mut Interface,
        current: SmoltcpInstant,
        packet_io: &mut StackDevice<'_>,
        sockets: &mut SocketSet<'_>,
        budget_packets: usize,
        round_started_at: MonotonicInstant,
    ) -> SmoltcpIngressProgress {
        let mut progress = SmoltcpIngressProgress::default();
        while progress.packets < budget_packets {
            if progress
                .packets
                .is_multiple_of(POLL_TIME_CHECK_INTERVAL_WORK_ITEMS)
                && progress.packets > 0
                && has_reached_poll_round_time_limit(round_started_at, monotonic_time())
            {
                progress.has_reached_time_limit = true;
                break;
            }

            match iface.poll_ingress_single(current, packet_io, sockets) {
                PollIngressSingleResult::None => break,
                PollIngressSingleResult::PacketProcessed => {}
                PollIngressSingleResult::SocketStateChanged => {
                    progress.has_socket_state_change = true;
                }
            }
            progress.packets += 1;
        }

        if progress.packets > 0
            && has_reached_poll_round_time_limit(round_started_at, monotonic_time())
        {
            progress.has_reached_time_limit = true;
        }
        progress
    }

    #[cfg(unittest)]
    pub(crate) fn ipv4_addrs_for_tests(&self) -> alloc::vec::Vec<SmoltcpIpCidr> {
        self.iface.lock().ip_addrs().to_vec()
    }

    #[cfg(unittest)]
    pub(crate) fn replace_for_tests(&self, io: &mut NetIo, mtu: usize) {
        *self.iface.lock() = Interface::new(
            smoltcp::iface::Config::new(HardwareAddress::Ip),
            &mut StackDevice::new(io, mtu),
            now(),
        );
        self.mtu.store(mtu, Ordering::Release);
        self.protocol_deadline_micros.store(0, Ordering::Release);
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::{vec, vec::Vec};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::buf::PacketOwner;
    #[def_test]
    fn smoltcp_ingress_keeps_packets_after_reaching_budget() {
        let current = SmoltcpInstant::from_millis(10);
        let mut packet_io = NetIo::new();
        let config = smoltcp::iface::Config::new(HardwareAddress::Ip);
        let mut iface =
            Interface::new(config, &mut StackDevice::new(&mut packet_io, 1500), current);
        let mut packets = Vec::from([
            PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::DeviceRx),
            PacketBuf::from_ip_packet_vec(1, vec![0x45, 0, 0, 20], PacketOwner::DeviceRx),
        ]);
        packet_io.enqueue_stack_ingress(&mut packets).unwrap();
        let mut sockets = SocketSet::new(vec![]);

        let progress = Stack::poll_ingress_budgeted(
            &mut iface,
            current,
            &mut StackDevice::new(&mut packet_io, 1500),
            &mut sockets,
            1,
            monotonic_time(),
        );

        assert_eq!(progress.packets, 1);
        assert!(packet_io.has_stack_ingress());
    }
}
