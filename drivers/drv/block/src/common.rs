// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared helpers for the block driver modules.

use driver_core::{ProbeError, RuntimeError, desc::DeviceLocation};

pub(crate) fn map_driver_error(err: device_traits::DriverError) -> ProbeError {
    let error = match err {
        device_traits::DriverError::InvalidInput | device_traits::DriverError::ReadOnly => {
            RuntimeError::Invalid
        }
        device_traits::DriverError::NoMemory => RuntimeError::NoMemory,
        device_traits::DriverError::ResourceBusy | device_traits::DriverError::AlreadyExists => {
            RuntimeError::Busy
        }
        device_traits::DriverError::BadState => RuntimeError::BadState,
        device_traits::DriverError::Io | device_traits::DriverError::WouldBlock => RuntimeError::Io,
        device_traits::DriverError::Unsupported => RuntimeError::Unsupported,
    };
    ProbeError::Fatal(error)
}

/// Physical sysfs path for a device location (parent path for the disk's
/// devnode publication — mirrors the virtio-side helper).
pub(crate) fn physical_path(location: DeviceLocation) -> alloc::string::String {
    devnode::physical_path(&location)
}
