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

use alloc::{boxed::Box, string::String};

use driver_base::ClassDevice;
use kerrno::KResult;

#[cfg(feature = "virtio-9p")]
use crate::Virtio9pDevice;

#[cfg(feature = "virtio-9p")]
impl Virtio9pDevice for ClassDevice<Box<dyn Virtio9pDevice + Send + Sync>> {
    fn mount_tag(&self) -> String {
        self.mount_tag()
    }

    fn request(&self, req: &[u8], resp: &mut [u8]) -> KResult<usize> {
        self.with_available(|device| device.request(req, resp))
    }
}
