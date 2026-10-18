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

use driver_base::{Device, DriverOps, PlatformBus, PlatformId, ProbeError};
use kerrno::KError;
use subsys_block::{
    Gendisk,
    class::{BLOCK, BlockDeviceImpl},
};

/// Map the block class's error vocabulary onto the probe control flow.
fn map_driver_error(err: KError) -> ProbeError {
    let error = match err {
        KError::InvalidInput | KError::ReadOnlyFilesystem => driver_base::RuntimeError::Invalid,
        KError::NoMemory => driver_base::RuntimeError::NoMemory,
        KError::ResourceBusy | KError::AlreadyExists => driver_base::RuntimeError::Busy,
        KError::BadState => driver_base::RuntimeError::BadState,
        KError::Io | KError::WouldBlock => driver_base::RuntimeError::Io,
        KError::Unsupported => driver_base::RuntimeError::Unsupported,
        // The class layer's vocabulary is the former DriverError set; any
        // other kerrno code is foreign here — treat it as an I/O failure.
        _ => driver_base::RuntimeError::Io,
    };
    ProbeError::Fatal(error)
}

/// `struct platform_device_id ramdisk_ids[]`: one board alias row.
static IDS: &[PlatformId] = &[PlatformId::Alias(driver_base::AliasMatch::new("ramdisk"))];

/// Driver-private data: the Gendisk stays owned by the block class
/// registry; nothing else to hold.
pub struct RamdiskData {
    /// The Data-owned publication: dropping unpublishes (first field —
    /// registry removal → del_gendisk + devnode withdraw).
    _publication: driver_base::Publication<BlockDeviceImpl>,
}

/// The driver ops.
pub struct RamdiskDriver;

impl DriverOps<PlatformBus> for RamdiskDriver {
    type Data = RamdiskData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<RamdiskData, ProbeError> {
        // `ramdisk-static` backs the device with a build-time filesystem
        // image (embedded, zero-copy) so it can be mounted as a real root
        // filesystem. Otherwise the device is a zero-filled region, useful
        // only for bring-up. The arms produce different concrete types, so
        // this must be cfg attributes rather than `if cfg!()`.
        #[cfg(feature = "ramdisk-static")]
        let (backend, name): (
            Box<dyn subsys_block::BlockDeviceOperations + Send + Sync>,
            alloc::string::String,
        ) = {
            let image = subsys_block::ramdisk_image::ramdisk();
            (Box::new(image), alloc::string::String::from("ramdisk"))
        };
        #[cfg(not(feature = "ramdisk-static"))]
        let (backend, name): (
            Box<dyn subsys_block::BlockDeviceOperations + Send + Sync>,
            alloc::string::String,
        ) = {
            // Capacity is a platform/build parameter (KFEAT_RAMDISK_SIZE,
            // Linux brd's rd_size): a synthetic device has no firmware
            // description to read it from.
            (
                Box::new(subsys_block::ramdisk_heap::HeapRamDisk::new(
                    kbuild_config::KFEAT_RAMDISK_SIZE as usize,
                )),
                alloc::string::String::from("ramdisk"),
            )
        };
        // Devnode context: without it the disk never reaches /dev, uevent
        // or sysfs (add_disk publishes only when the parent path is set) —
        // the same contract every block driver follows (virtio-blk).
        let parent_path = alloc::string::String::from(dev.info().path());
        let modalias = dev.info().modalias().map(alloc::string::String::from);
        let disk = Gendisk::new(name, 1, 0, 1, backend)
            .map_err(map_driver_error)?
            .with_devnode_context(&parent_path, modalias);
        let publication = dev
            .publish(&BLOCK, Arc::new(disk), "ramdisk")
            .map_err(map_driver_error)?;
        Ok(RamdiskData {
            _publication: publication,
        })
    }

    fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut RamdiskData) {
        // The Data-owned publication guard unpublishes at Data drop.
    }
}

driver_base::declare_driver!(PlatformBus, RamdiskDriver, "ramdisk", IDS);
