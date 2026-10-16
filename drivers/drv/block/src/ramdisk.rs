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

use device_class::{ClassParent, publish_block};
use driver_base::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError,
    platform::{PlatformBus, PlatformId},
};
use subsys_block::Gendisk;

/// `struct platform_device_id ramdisk_ids[]`: one board alias row.
static IDS: &[PlatformId] = &[PlatformId::Alias(driver_base::platform::AliasMatch::new(
    "ramdisk",
))];

/// Driver-private data: the Gendisk stays owned by the block class
/// registry; nothing else to hold.
pub struct RamdiskData {
    /// The Data-owned publication: dropping unpublishes (first field —
    /// registry removal → del_gendisk + devnode withdraw).
    _publication: device_class::ClassWithdraw,
}

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
            Box<dyn subsys_block::BlockDeviceOperations + Send + Sync>,
            alloc::string::String,
        ) = {
            let dev = subsys_block::ramdisk_image::ramdisk();
            (Box::new(dev), alloc::string::String::from("ramdisk"))
        };
        #[cfg(not(feature = "ramdisk-static"))]
        let (dev, name): (
            Box<dyn subsys_block::BlockDeviceOperations + Send + Sync>,
            alloc::string::String,
        ) = {
            // Capacity is a platform/build parameter (KFEAT_RAMDISK_SIZE,
            // Linux brd's rd_size): a synthetic device has no firmware
            // description to read it from.
            (
                Box::new(subsys_block::ramdisk_heap::RamDisk::new(
                    kbuild_config::KFEAT_RAMDISK_SIZE as usize,
                )),
                alloc::string::String::from("ramdisk"),
            )
        };
        // Devnode context: without it the disk never reaches /dev, uevent
        // or sysfs (add_disk publishes only when the parent path is set) —
        // the same contract every block driver follows (virtio-blk).
        let parent_path = crate::common::physical_path(dev);
        let modalias = _dev.modalias().map(alloc::string::String::from);
        let disk = Gendisk::new(name, 1, 0, 1, dev)
            .map_err(crate::common::map_driver_error)?
            .with_devnode_context(&parent_path, modalias);
        let location = crate::common::physical_path(_dev);
        let publication = publish_block(
            ClassParent::detached(_dev.id().raw(), "ramdisk", location),
            Arc::new(disk),
        )
        .map_err(crate::common::map_driver_error)?;
        Ok(RamdiskData {
            _publication: device_class::block_withdraw(&publication),
        })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, RamdiskData>) {
        // The Data-owned publication guard unpublishes at Data drop.
    }
}

/// `module_platform_driver()` equivalent.
pub fn register() {
    driver_base::global_domain().register_driver(DriverDesc::new("ramdisk", IDS, RamdiskDriver));
}
