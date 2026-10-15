// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The consumer-facing half of this subsystem: its device trait
//! implemented for the class device (`ClassDevice<Box<dyn Trait>>`) —
//! admission-gated data-plane forwarding over the shared runtime.
//! Lives HERE, with the trait it forwards to (the orphan rule agrees
//! with the layering: a subsystem knows both its trait and the model's
//! class device, like drivers/block knows struct device).

extern crate alloc;

use alloc::{boxed::Box, sync::Arc};

use driver_base::ClassDevice;
use kerrno::KResult;

use super::{MacAddress, NetBufHandle, NetDevice, NetRxScheduler};

impl NetDevice for ClassDevice<Box<dyn NetDevice + Send + Sync>> {
    fn mac(&self) -> MacAddress {
        self.with(|device| device.mac())
    }

    fn can_tx(&self) -> bool {
        // Linux netif_tx_disable parity: after the offline flag flips the
        // queue reports stopped, so the network stack's poll loop stops
        // selecting this NIC instead of retrying `send` into `BadState`.
        self.is_available() && self.with(|device| device.can_tx())
    }

    fn can_rx(&self) -> bool {
        self.is_available() && self.with(|device| device.can_rx())
    }

    fn rx_queue_len(&self) -> usize {
        self.with(|device| device.rx_queue_len())
    }

    fn tx_queue_len(&self) -> usize {
        self.with(|device| device.tx_queue_len())
    }

    fn recycle_rx(&self, rx_buf: NetBufHandle) -> KResult {
        self.with_available(|device| device.recycle_rx(rx_buf))
    }

    fn recycle_tx(&self) -> KResult {
        self.with_available(|device| device.recycle_tx())
    }

    fn send(&self, tx_buf: NetBufHandle) -> KResult {
        self.with_available(|device| device.send(tx_buf))
    }

    fn recv(&self) -> KResult<NetBufHandle> {
        self.with_available(|device| device.recv())
    }

    fn alloc_tx_buf(&self, size: usize) -> KResult<NetBufHandle> {
        self.with_available(|device| device.alloc_tx_buf(size))
    }

    fn set_rx_scheduler(&self, scheduler: Option<Arc<dyn NetRxScheduler>>) -> KResult {
        self.with_available(|device| device.set_rx_scheduler(scheduler))
    }
}
