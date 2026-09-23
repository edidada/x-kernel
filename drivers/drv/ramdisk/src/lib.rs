// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ramdisk driver on the driver-core model (platform-static bus).
//!
//! The simplest complete template: a board-static device with no firmware
//! resources, matched by alias, publishing one block device. Under
//! `ramdisk-static` the disk is backed by the build-time embedded image
//! (mountable root filesystem); otherwise it is a zero-filled region.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, sync::Arc};

use block::Gendisk;
use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError,
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
        let disk = Gendisk::new(name, 1, 0, 1, dev).map_err(map_driver_error)?;
        let location = legacy_location(_dev.descriptor().location);
        publish_block(ClassParent::detached("ramdisk", location), Arc::new(disk))
            .map_err(map_driver_error)?;
        Ok(RamdiskData)
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, RamdiskData>) {}
}

fn legacy_location(location: driver_core::desc::DeviceLocation) -> kdevice::DeviceLocation {
    use driver_core::desc::DeviceLocation as Ours;
    use kdevice::DeviceLocation as Legacy;
    match location {
        Ours::Pci {
            segment,
            bus,
            device,
            function,
        } => Legacy::Pci {
            segment,
            bus,
            device,
            function,
        },
        Ours::Mmio { base, size } => Legacy::Mmio { base, size },
        Ours::FirmwareNode { id } => Legacy::FirmwareNode { id },
        Ours::PlatformStatic { id } => Legacy::PlatformStatic { id },
        Ours::Bridge { domain } => Legacy::Bridge { domain },
    }
}

fn map_driver_error(err: driver_base::DriverError) -> ProbeError {
    let error = match err {
        driver_base::DriverError::InvalidInput | driver_base::DriverError::ReadOnly => {
            RuntimeError::Invalid
        }
        driver_base::DriverError::NoMemory => RuntimeError::NoMemory,
        driver_base::DriverError::ResourceBusy | driver_base::DriverError::AlreadyExists => {
            RuntimeError::Busy
        }
        driver_base::DriverError::BadState => RuntimeError::BadState,
        driver_base::DriverError::Io | driver_base::DriverError::WouldBlock => RuntimeError::Io,
        driver_base::DriverError::Unsupported => RuntimeError::Unsupported,
    };
    ProbeError::Fatal(error)
}

/// `module_platform_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new("ramdisk", IDS, RamdiskDriver));
}
