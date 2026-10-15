// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! smoltcp interface lifecycle and admitted input, independent of routing.

use alloc::{collections::VecDeque, vec::Vec};

use smoltcp::{
    iface::{Context, Interface, PollIngressSingleResult, PollResult, SocketSet},
    time::Instant,
    wire::{HardwareAddress, IpCidr},
};

use super::device::StackDevice;
use crate::{buf::PacketBuf, consts::SOCKET_BUFFER_SIZE, runtime::tx_queue::DataTxQueue};

/// Owns smoltcp state and pending input under one mutex.
///
/// Shared output is borrowed per operation. Callers acquire the socket set and
/// TX queue before this backend when needed; backend holders never acquire Router.
pub(crate) struct SmoltcpStack {
    iface: Interface,
    rx: VecDeque<PacketBuf>,
    mtu: usize,
}

impl SmoltcpStack {
    pub(crate) fn new(mtu: usize, tx: &mut DataTxQueue, now: Instant) -> Self {
        let mut rx = VecDeque::with_capacity(SOCKET_BUFFER_SIZE);
        let iface = Interface::new(
            smoltcp::iface::Config::new(HardwareAddress::Ip),
            &mut StackDevice::new(&mut rx, tx, mtu),
            now,
        );
        Self { iface, rx, mtu }
    }

    pub(crate) fn enqueue_ingress_packets(&mut self, packets: &mut Vec<PacketBuf>) {
        debug_assert!(packets.len() <= self.ingress_capacity());
        self.rx.extend(packets.drain(..));
    }

    pub(crate) fn ingress_capacity(&self) -> usize {
        SOCKET_BUFFER_SIZE.saturating_sub(self.rx.len())
    }

    pub(crate) fn has_pending_ingress(&self) -> bool {
        !self.rx.is_empty()
    }

    pub(crate) fn poll_maintenance(&mut self, now: Instant) {
        self.iface.poll_maintenance(now);
    }

    pub(crate) fn poll_ingress_single(
        &mut self,
        now: Instant,
        tx: &mut DataTxQueue,
        sockets: &mut SocketSet<'_>,
    ) -> PollIngressSingleResult {
        self.iface.poll_ingress_single(
            now,
            &mut StackDevice::new(&mut self.rx, tx, self.mtu),
            sockets,
        )
    }

    pub(crate) fn poll_egress(
        &mut self,
        now: Instant,
        tx: &mut DataTxQueue,
        sockets: &mut SocketSet<'_>,
    ) -> PollResult {
        self.iface.poll_egress(
            now,
            &mut StackDevice::new(&mut self.rx, tx, self.mtu),
            sockets,
        )
    }

    pub(crate) fn poll_at(&mut self, now: Instant, sockets: &SocketSet<'_>) -> Option<Instant> {
        self.iface.poll_at(now, sockets)
    }

    /// Borrows the protocol context for socket operations such as TCP connect.
    pub(crate) fn context(&mut self) -> &mut Context {
        self.iface.context()
    }

    pub(crate) fn update_local_ipv4_addrs(&mut self, addrs: &[crate::ip::Ipv4Cidr]) {
        self.iface.update_ip_addrs(|ip_addrs| {
            ip_addrs.clear();
            for addr in addrs {
                ip_addrs
                    .push(IpCidr::Ipv4(smoltcp::wire::Ipv4Cidr::new(
                        addr.address().into(),
                        addr.prefix_len(),
                    )))
                    .expect("Router validates the smoltcp address capacity");
            }
        });
    }

    /// Rebuilds cached capabilities while retaining addresses and pending input.
    pub(crate) fn rebuild(&mut self, mtu: usize, tx: &mut DataTxQueue, now: Instant) {
        let mut iface = Interface::new(
            smoltcp::iface::Config::new(HardwareAddress::Ip),
            &mut StackDevice::new(&mut self.rx, tx, mtu),
            now,
        );
        iface.update_ip_addrs(|addrs| {
            for addr in self.iface.ip_addrs() {
                addrs
                    .push(*addr)
                    .expect("rebuilt interface preserves the original address capacity");
            }
        });
        self.iface = iface;
        self.mtu = mtu;
    }

    #[cfg(unittest)]
    pub(crate) fn ip_addrs(&self) -> &[IpCidr] {
        self.iface.ip_addrs()
    }
}
