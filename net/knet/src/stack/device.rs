// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Temporary smoltcp adapter borrowing `NetIo` queues while that mutex is held.
use alloc::vec;

use smoltcp::{
    phy::{DeviceCapabilities, Medium},
    time::Instant,
};

use crate::{buf::PacketBuf, io::NetIo};

const STACK_MAX_BURST_SIZE: usize = 64;

pub(crate) struct StackDevice<'a> {
    io: &'a mut NetIo,
    mtu: usize,
}
impl<'a> StackDevice<'a> {
    pub(crate) fn new(io: &'a mut NetIo, mtu: usize) -> Self {
        Self { io, mtu }
    }
}
pub struct TxToken<'a>(&'a mut NetIo);

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        debug_assert!(self.0.can_transmit_from_stack());

        let mut ip_packet = vec![0u8; len];
        let result = f(&mut ip_packet);
        self.0.enqueue_stack_tx(ip_packet);
        result
    }
}

pub struct RxToken(PacketBuf);

impl smoltcp::phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self
            .0
            .network_packet()
            .expect("NetIo RX queue only contains IP packets"))
    }
}

impl smoltcp::phy::Device for StackDevice<'_> {
    type RxToken<'a>
        = RxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if !self.io.can_receive_from_stack() {
            return None;
        }
        let packet = self.io.pop_stack_ingress()?;
        Some((RxToken(packet), TxToken(self.io)))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if !self.io.can_transmit_from_stack() {
            None
        } else {
            Some(TxToken(self.io))
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps.max_burst_size = Some(STACK_MAX_BURST_SIZE);
        caps
    }
}
