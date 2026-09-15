// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Temporary smoltcp adapter borrowing transport queues for one transaction.
use alloc::{collections::VecDeque, vec};

use smoltcp::{
    phy::{DeviceCapabilities, Medium},
    time::Instant,
};

use crate::{
    buf::{PacketBuf, PacketOwner},
    consts::SOCKET_BUFFER_SIZE,
    datapath::{NetIo, io::DATA_TX_QUEUE_SIZE},
};
pub(crate) struct StackDevice<'a> {
    rx_queue: &'a mut VecDeque<PacketBuf>,
    data_tx_queue: &'a mut VecDeque<PacketBuf>,
    mtu: usize,
}
impl<'a> StackDevice<'a> {
    pub(crate) fn new(io: &'a mut NetIo, mtu: usize) -> Self {
        Self {
            rx_queue: &mut io.rx_queue,
            data_tx_queue: &mut io.data_tx_queue,
            mtu,
        }
    }
}
pub struct TxToken<'a>(&'a mut VecDeque<PacketBuf>);

impl smoltcp::phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        debug_assert!(self.0.len() < SOCKET_BUFFER_SIZE);

        let mut ip_packet = vec![0u8; len];
        let result = f(&mut ip_packet);
        self.0.push_back(PacketBuf::from_ip_packet_vec(
            0,
            ip_packet,
            PacketOwner::Ipv4Stack,
        ));
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
        if self.rx_queue.is_empty() || self.data_tx_queue.len() >= DATA_TX_QUEUE_SIZE {
            None
        } else {
            Some((
                RxToken(self.rx_queue.pop_front().unwrap()),
                TxToken(self.data_tx_queue),
            ))
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.data_tx_queue.len() >= DATA_TX_QUEUE_SIZE {
            None
        } else {
            Some(TxToken(self.data_tx_queue))
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps.max_burst_size = Some(SOCKET_BUFFER_SIZE);
        caps
    }
}
