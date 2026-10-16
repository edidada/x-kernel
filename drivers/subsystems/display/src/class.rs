// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `display` class: one const-constructed static plus the admission-gated
//! consumer data plane (the Linux `struct class display` analogue).

use alloc::boxed::Box;

use devnode::NodeClass;
use driver_base::{
    ClassDevice,
    class::class_api::{Class, ClassHooks},
};
use kerrno::KResult;

use super::{DisplayDevice, DisplayInfo, ScanoutRect, ScanoutResource};

pub type DisplayDeviceImpl = Box<dyn DisplayDevice + Send + Sync>;

pub static DISPLAY: Class<DisplayDeviceImpl, DisplayHooks> = Class::new(DisplayHooks);

pub struct DisplayHooks;

impl ClassHooks<DisplayDeviceImpl> for DisplayHooks {
    fn node_class(&self, _runtime: &DisplayDeviceImpl) -> Option<NodeClass> {
        Some(NodeClass::Drm)
    }
}

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
