// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `virtio-9p` class: one const-constructed static over the family's
//! own trait — the 9p "subsystem" lives with the family (no standalone
//! subsystem crate), so its class does too.

use alloc::boxed::Box;

use driver_base::{Publication, class::class_api::Class};
use kerrno::KResult;

use crate::Virtio9pDevice;

/// The 9p class payload: one boxed device.
pub type Virtio9pDeviceImpl = Box<dyn Virtio9pDevice + Send + Sync>;

/// The virtio-9p device class.
pub static P9: Class<Virtio9pDeviceImpl> = Class::new(());

// Consumer-facing data plane: every operation forwarded through the
// publication goes through the availability gate, like every other class.
impl Virtio9pDevice for Publication<Virtio9pDeviceImpl> {
    fn mount_tag(&self) -> alloc::string::String {
        self.with(|device| device.mount_tag())
    }

    fn request(&self, request: &[u8], response: &mut [u8]) -> KResult<usize> {
        self.with_available(|device| device.request(request, response))
    }
}
