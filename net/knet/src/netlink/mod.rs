// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal netlink socket support for in-kernel consumers.
//!
//! This provides a small AF_NETLINK socket that is sufficient to grow a
//! rtnetlink implementation for Kata. The current dispatcher intentionally keeps
//! the protocol surface small and returns netlink errors for unsupported
//! requests, so later work can extend request handling without changing the
//! socket plumbing.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};

use ::core::sync::atomic::AtomicU64;
use ksync::{Mutex, RwLock, static_lock};

use crate::{SERVICE, general::GeneralOptions};

mod rtnetlink;
mod socket;
#[cfg(unittest)]
mod tests;
mod wire;

pub use socket::{NetlinkSocket, publish_kobject_uevent};
use wire::{
    ARPHRD_ETHER, ARPHRD_LOOPBACK, NLM_F_ACK, NLM_F_CREATE, NLM_F_EXCL, NLM_F_MULTI, NLM_F_REPLACE,
    NLM_F_REQUEST, NLMSG_HDR_LEN, RTM_DELADDR, RTM_DELROUTE, RTM_GETADDR, RTM_GETLINK,
    RTM_GETROUTE, RTM_NEWADDR, RTM_NEWLINK, RTM_NEWNEIGH, RTM_NEWROUTE,
};

pub const NETLINK_ROUTE: i32 = 0;
pub(super) const NETLINK_KOBJECT_UEVENT: i32 = 15;
static_lock! {
    pub(super) static UEVENT_SUBSCRIBERS: Mutex<Vec<Weak<NetlinkSocketInner>>> =
        Mutex::new(Vec::new());
}
pub(super) static UEVENT_SEQNUM: AtomicU64 = AtomicU64::new(0);
pub(super) const NETLINK_RX_QUEUE_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct NetlinkAddr {
    pub pid: u32,
    pub groups: u32,
}

#[derive(Clone)]
pub(super) struct NetlinkPacket {
    pub(super) from: NetlinkAddr,
    pub(super) data: Vec<u8>,
}

#[derive(Default)]
pub(super) struct NetlinkRxQueue {
    packets: VecDeque<NetlinkPacket>,
    bytes: usize,
}

impl NetlinkRxQueue {
    pub(super) fn push_back(&mut self, packet: NetlinkPacket) -> bool {
        let packet_len = packet.data.len();
        if packet_len > NETLINK_RX_QUEUE_LIMIT
            || self.bytes.saturating_add(packet_len) > NETLINK_RX_QUEUE_LIMIT
        {
            return false;
        }
        self.bytes += packet_len;
        self.packets.push_back(packet);
        true
    }

    pub(super) fn pop_front(&mut self) -> Option<NetlinkPacket> {
        let packet = self.packets.pop_front()?;
        self.bytes = self.bytes.saturating_sub(packet.data.len());
        Some(packet)
    }

    pub(super) fn front(&self) -> Option<&NetlinkPacket> {
        self.packets.front()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    pub(super) fn can_push_bytes(&self, bytes: usize) -> bool {
        bytes <= NETLINK_RX_QUEUE_LIMIT
            && self.bytes.saturating_add(bytes) <= NETLINK_RX_QUEUE_LIMIT
    }
}

pub(super) struct NetlinkSocketInner {
    pub(super) protocol: i32,
    pub(super) local_addr: RwLock<Option<NetlinkAddr>>,
    pub(super) rx_queue: Mutex<NetlinkRxQueue>,
    pub(super) poll_rx: Arc<kpoll::PollSet>,
    pub(super) general: GeneralOptions,
}
