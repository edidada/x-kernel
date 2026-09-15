// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared synthetic device for configuration and output boundary tests.
use alloc::{string::String, vec::Vec};
use core::task::Waker;

use kerrno::LinuxError;
use ktime_types::MonotonicInstant;

use super::{
    IF_OPER_DOWN, IF_OPER_UNKNOWN, IF_OPER_UP, LINK_FLAG_LOWER_UP, LINK_FLAG_RUNNING, LINK_FLAG_UP,
    LinkKind, LinkSendSnapshot, LinkSnapshot, NeighborUpdate, NetDevice,
};
use crate::buf::PacketBuf;
pub(crate) struct TestDevice {
    link: LinkSnapshot,
    device_id: Option<kdevice::DeviceId>,
    neighbors: Vec<crate::ip::IpAddress>,
}

impl TestDevice {
    pub(crate) fn new(link: LinkSnapshot) -> Self {
        Self {
            link,
            device_id: None,
            neighbors: Vec::new(),
        }
    }

    pub(crate) fn with_device_id(mut self, device_id: kdevice::DeviceId) -> Self {
        self.device_id = Some(device_id);
        self
    }
}

impl NetDevice for TestDevice {
    fn name(&self) -> &str {
        &self.link.name
    }

    fn link_kind(&self) -> LinkKind {
        self.link.kind
    }

    fn mtu(&self) -> usize {
        self.link.mtu
    }

    fn is_link_up(&self) -> bool {
        self.link.flags & LINK_FLAG_UP != 0
    }

    fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot {
        LinkSnapshot {
            ifindex,
            ..self.link.clone()
        }
    }

    fn link_send_snapshot(&self) -> LinkSendSnapshot {
        LinkSendSnapshot {
            is_up: self.is_link_up(),
            mtu: self.link.mtu,
            hardware_addr: self.link.hardware_addr,
        }
    }

    fn device_id(&self) -> Option<kdevice::DeviceId> {
        self.device_id
    }

    fn poll_rx(&mut self, _ifindex: i32, _timestamp: MonotonicInstant) -> Option<PacketBuf> {
        None
    }

    fn has_rx_work(&self) -> bool {
        false
    }

    fn send_ip_packet(
        &mut self,
        _ifindex: i32,
        _next_hop: crate::ip::IpAddress,
        _source_addr: crate::ip::IpAddress,
        _packet: PacketBuf,
        _timestamp: MonotonicInstant,
    ) -> bool {
        false
    }

    fn register_rx_waker(
        &self,
        _source_waker: &Waker,
        _context: &mut kpoll::PollContext<'_>,
    ) -> Result<(), kpoll::PollRegisterError> {
        Ok(())
    }

    fn set_name(&mut self, name: String) {
        self.link.name = name;
    }

    fn set_mtu(&mut self, mtu: usize) -> Result<(), LinuxError> {
        self.link.kind.validate_mtu(mtu)?;
        self.link.mtu = mtu;
        Ok(())
    }

    fn set_link_up(&mut self, is_up: bool) {
        if is_up {
            self.link.flags |= LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP;
            self.link.operstate = if self.link.kind == LinkKind::Ethernet {
                IF_OPER_UP
            } else {
                IF_OPER_UNKNOWN
            };
        } else {
            self.link.flags &= !(LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP);
            self.link.operstate = IF_OPER_DOWN;
        }
    }

    fn apply_neighbor_update(&mut self, update: NeighborUpdate) -> Result<(), LinuxError> {
        if !self.neighbors.contains(&update.dst) {
            self.neighbors.push(update.dst);
        }
        Ok(())
    }

    fn has_neighbor(&self, dst: crate::ip::IpAddress) -> bool {
        self.neighbors.contains(&dst)
    }
}

impl TestDevice {
    pub(crate) fn ethernet(id: u64, mtu: usize) -> Self {
        Self::new(LinkSnapshot {
            ifindex: 0,
            name: String::from("test"),
            flags: LINK_FLAG_UP | LINK_FLAG_RUNNING | LINK_FLAG_LOWER_UP,
            mtu,
            operstate: IF_OPER_UP,
            kind: LinkKind::Ethernet,
            hardware_addr: [0; 6],
            broadcast_addr: [0xff; 6],
        })
        .with_device_id(kdevice::DeviceId::new(id))
    }
}
