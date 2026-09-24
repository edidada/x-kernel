// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared helpers for the virtio driver modules (error mapping, class
//! parent location conversion) — one copy instead of ten.

use alloc::{format, string::String};

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

/// Format the DEVPATH physical path from a location (the same string the
/// devnode registry stores).
pub(crate) fn physical_path(location: DeviceLocation) -> String {
    use devnode::physical_path_pci;
    match location {
        DeviceLocation::Pci {
            segment,
            bus,
            device,
            function,
        } => physical_path_pci(segment, bus, device, function),
        DeviceLocation::Mmio { base, .. } => format!("platform/{base:x}.mmio"),
        DeviceLocation::FirmwareNode { id } => format!("platform/firmware{id}"),
        DeviceLocation::PlatformStatic { id } => format!("platform/static{id}"),
        DeviceLocation::Bridge { domain } => format!("pci{domain:04x}:00"),
    }
}

pub(crate) fn map_res_error(err: device_res::ResError) -> ProbeError {
    let error = match err {
        device_res::ResError::InvalidResource => RuntimeError::Invalid,
        device_res::ResError::MappingFailed => RuntimeError::Io,
        device_res::ResError::NoMemory => RuntimeError::NoMemory,
        device_res::ResError::Busy => RuntimeError::Busy,
        device_res::ResError::Unsupported => RuntimeError::Unsupported,
    };
    ProbeError::Fatal(error)
}
