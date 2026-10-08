// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared helpers for the block driver modules.

use devnode::DeviceLocation as LegacyLocation;
use driver_core::{ProbeError, RuntimeError, desc::DeviceLocation};

pub(crate) fn legacy_location(location: DeviceLocation) -> LegacyLocation {
    use driver_core::desc::DeviceLocation as Ours;
    match location {
        Ours::Pci {
            segment,
            bus,
            device,
            function,
        } => LegacyLocation::Pci {
            segment,
            bus,
            device,
            function,
        },
        Ours::Mmio { base, size } => LegacyLocation::Mmio { base, size },
        Ours::FirmwareNode { id } => LegacyLocation::FirmwareNode { id },
        Ours::PlatformStatic { id } => LegacyLocation::PlatformStatic { id },
        Ours::Bridge { domain } => LegacyLocation::Bridge { domain },
    }
}

pub(crate) fn map_driver_error(err: driver_base::DriverError) -> ProbeError {
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

/// Physical sysfs path for a device location (parent path for the disk's
/// devnode publication — mirrors the virtio-side helper).
pub(crate) fn physical_path(location: DeviceLocation) -> alloc::string::String {
    match location {
        DeviceLocation::Pci {
            segment,
            bus,
            device,
            function,
        } => devnode::physical_path_pci(segment, bus, device, function),
        DeviceLocation::Mmio { base, .. } => alloc::format!("platform/{base:x}.mmio"),
        DeviceLocation::FirmwareNode { id } => alloc::format!("platform/firmware{id}"),
        DeviceLocation::PlatformStatic { id } => alloc::format!("platform/static{id}"),
        DeviceLocation::Bridge { domain } => alloc::format!("pci{domain:04x}:00"),
    }
}
