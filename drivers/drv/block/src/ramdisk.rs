// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ramdisk driver on the driver-core model (platform-static bus).
//!
//! The simplest complete template: a board-static device with no firmware
//! resources, matched by alias, publishing one block device. Under
//! `ramdisk-static` the disk is backed by the build-time embedded image
//! (mountable root filesystem); otherwise it is a zero-filled region.

use alloc::{boxed::Box, sync::Arc};

use block::Gendisk;
use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError,
    platform::{PlatformBus, PlatformId},
};
use kclass::{ClassParent, publish_block};

/// `struct platform_device_id ramdisk_ids[]`: one board alias row.
static IDS: &[PlatformId] = &[PlatformId::Alias(driver_core::platform::AliasMatch::new(
    "ramdisk",
))];

/// Driver-private data: the Gendisk stays owned by the block class
/// registry; nothing else to hold.
pub struct RamdiskData;

/// The driver ops.
pub struct RamdiskDriver;

impl DriverOps<PlatformBus> for RamdiskDriver {
    type Data = RamdiskData;

    fn probe(&self, _dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<RamdiskData, ProbeError> {
        // `ramdisk-static` backs the device with a build-time filesystem
        // image (embedded, zero-copy) so it can be mounted as a real root
        // filesystem. Otherwise the device is a zero-filled region, useful
        // only for bring-up. The arms produce different concrete types, so
        // this must be cfg attributes rather than `if cfg!()`.
        #[cfg(feature = "ramdisk-static")]
        let (dev, name): (
            Box<dyn block::BlockDeviceOperations + Send + Sync>,
            alloc::string::String,
        ) = {
            let dev = block::ramdisk_image::ramdisk();
            (Box::new(dev), alloc::string::String::from("ramdisk"))
        };
        #[cfg(not(feature = "ramdisk-static"))]
        let (dev, name): (
            Box<dyn block::BlockDeviceOperations + Send + Sync>,
            alloc::string::String,
        ) = {
            // 16 MiB zero-filled region.
            (
                Box::new(block::ramdisk::RamDisk::new(0x100_0000)),
                alloc::string::String::from("ramdisk"),
            )
        };
        // Devnode context: without it the disk never reaches /dev, uevent
        // or sysfs (add_disk publishes only when the parent path is set) —
        // the same contract every block driver follows (virtio-blk).
        let parent_path = crate::common::physical_path(_dev.descriptor().location);
        let modalias = _dev.modalias().map(alloc::string::String::from);
        let disk = Gendisk::new(name, 1, 0, 1, dev)
            .map_err(crate::common::map_driver_error)?
            .with_devnode_context(&parent_path, modalias);
        let location = _dev.descriptor().location;
        publish_block(
            ClassParent::detached(_dev.id().raw(), "ramdisk", location),
            Arc::new(disk),
        )
        .map_err(crate::common::map_driver_error)?;
        Ok(RamdiskData)
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, RamdiskData>) {}
}

/// `module_platform_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new("ramdisk", IDS, RamdiskDriver));
}
