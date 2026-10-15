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

use super::{DisplayDevice, DisplayInfo, ScanoutRect, ScanoutResource};

impl DisplayDevice for ClassDevice<Box<dyn DisplayDevice + Send + Sync>> {
    fn info(&self) -> DisplayInfo {
        self.with(|device| device.info())
    }

    fn need_flush(&self) -> bool {
        self.with(|device| device.need_flush())
    }

    fn flush(&self) -> KResult {
        self.with_available(|device| device.flush())
    }

    fn create_scanout_resource(
        &self,
        resource: ScanoutResource,
        paddr: u64,
        length: u32,
    ) -> KResult {
        self.with_available(|device| device.create_scanout_resource(resource, paddr, length))
    }

    fn destroy_scanout_resource(&self, resource_id: u32) -> KResult {
        self.with_available(|device| device.destroy_scanout_resource(resource_id))
    }

    fn present_scanout_resource(&self, resource_id: u32, rect: ScanoutRect) -> KResult {
        self.with_available(|device| device.present_scanout_resource(resource_id, rect))
    }
}
