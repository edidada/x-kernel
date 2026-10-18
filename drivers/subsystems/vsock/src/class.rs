// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `vsock` class: one const-constructed static plus the admission-gated
//! consumer data plane (the Linux `struct class vsock` analogue).

use alloc::boxed::Box;

use driver_base::{Publication, class::class_api::Class};
use kerrno::KResult;

use super::{VsockConnectionInfo, VsockDevice, VsockTransportEvent};

pub type VsockDeviceImpl = Box<dyn VsockDevice + Send + Sync>;

pub static VSOCK: Class<VsockDeviceImpl> = Class::new(());

impl VsockDevice for Publication<Box<dyn VsockDevice + Send + Sync>> {
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
