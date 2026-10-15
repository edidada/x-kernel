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

use alloc::boxed::Box;

use driver_base::ClassDevice;
use kerrno::KResult;

use super::{VsockConnectionInfo, VsockDevice, VsockTransportEvent};

impl VsockDevice for ClassDevice<Box<dyn VsockDevice + Send + Sync>> {
    fn guest_cid(&self) -> u64 {
        self.with(|device| device.guest_cid())
    }

    fn listen(&self, port: u32) {
        self.with(|device| device.listen(port));
    }

    fn unlisten(&self, port: u32) {
        self.with(|device| device.unlisten(port));
    }

    fn connect(&self, info: &VsockConnectionInfo) -> KResult<()> {
        self.with_available(|device| device.connect(info))
    }

    fn accept(&self, info: &VsockConnectionInfo) -> KResult<()> {
        self.with_available(|device| device.accept(info))
    }

    fn force_close(&self, info: &VsockConnectionInfo) -> KResult<()> {
        self.with_available(|device| device.force_close(info))
    }

    fn send(&self, info: &VsockConnectionInfo, buf: &[u8]) -> KResult<usize> {
        self.with_available(|device| device.send(info, buf))
    }

    fn shutdown(&self, info: &VsockConnectionInfo) -> KResult<()> {
        self.with_available(|device| device.shutdown(info))
    }

    fn credit_update(&self, info: &VsockConnectionInfo) -> KResult<()> {
        self.with_available(|device| device.credit_update(info))
    }

    fn poll_event(
        &self,
        handler: &mut dyn FnMut(VsockTransportEvent, &[u8]) -> KResult<()>,
    ) -> KResult<bool> {
        self.with_available(|device| device.poll_event(handler))
    }
}
