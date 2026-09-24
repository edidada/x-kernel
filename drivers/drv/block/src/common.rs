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
