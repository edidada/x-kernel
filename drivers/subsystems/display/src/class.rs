// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `display` class: one const-constructed static plus the admission-gated
//! consumer data plane (the Linux `struct class display` analogue).

use alloc::{boxed::Box, format, string::String, vec};

use devnode::{DevNodeType, PublishError, publish_auto_indexed, withdraw};
use driver_base::{
    Publication,
    class::class_api::{Class, ClassHooks, PublicationNode},
};
use kerrno::KResult;

use super::{DisplayDevice, DisplayInfo, ScanoutRect, ScanoutResource};

pub type DisplayDeviceImpl = Box<dyn DisplayDevice + Send + Sync>;

pub static DISPLAY: Class<DisplayDeviceImpl, DisplayHooks> = Class::new(DisplayHooks);

pub struct DisplayHooks;

/// The DRM node type (`cardN`): the class owns how it projects.
static DRM_NODE: DevNodeType = DevNodeType::new(
    "drm",
    226,
    0,
    64,
    |index| format!("dri/card{index}"),
    |physical, index| format!("{physical}/drm/card{index}"),
);

impl ClassHooks<DisplayDeviceImpl> for DisplayHooks {
    fn publish(&self, _runtime: &DisplayDeviceImpl, node: &PublicationNode) -> KResult<()> {
        // udev rules match DRM nodes on DEVTYPE=drm_minor (permissions,
        // device ownership); losing it changes /dev/dri/cardN handling.
        let properties = vec![(String::from("DEVTYPE"), String::from("drm_minor"))];
        match publish_auto_indexed(
            node.id.raw(),
            &DRM_NODE,
            node.parent_path.as_str(),
            None,
            properties,
            Some(node.driver_name.clone()),
        ) {
            Ok(_) => {}
            Err(PublishError::ResourceBusy) => {
                log::error!(
                    "drm: card index space exhausted ({}); device gets no /dev node and no uevent",
                    DRM_NODE.index_count
                );
            }
            Err(PublishError::AlreadyExists) => {
                log::error!(
                    "drm: devnode id {} already published; skipping",
                    node.id.raw()
                );
            }
        }
        Ok(())
    }

    fn unpublish(&self, _runtime: &DisplayDeviceImpl, node: &PublicationNode) {
        withdraw(node.id.raw());
    }
}

impl DisplayDevice for Publication<Box<dyn DisplayDevice + Send + Sync>> {
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
