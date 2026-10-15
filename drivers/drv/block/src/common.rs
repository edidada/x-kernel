// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared helpers for the block driver modules.

use driver_base::{ProbeError, RuntimeError, desc::DeviceLocation};
use kerrno::KError;

pub(crate) fn map_driver_error(err: KError) -> ProbeError {
    let error = match err {
        KError::InvalidInput | KError::ReadOnlyFilesystem => RuntimeError::Invalid,
        KError::NoMemory => RuntimeError::NoMemory,
        KError::ResourceBusy | KError::AlreadyExists => RuntimeError::Busy,
        KError::BadState => RuntimeError::BadState,
        KError::Io | KError::WouldBlock => RuntimeError::Io,
        KError::Unsupported => RuntimeError::Unsupported,
        // The class layer's vocabulary is the former DriverError set; any
        // other kerrno code is foreign here — treat it as an I/O failure.
        _ => RuntimeError::Io,
    };
    ProbeError::Fatal(error)
}

/// Physical sysfs path for a device location (parent path for the disk's
/// devnode publication — mirrors the virtio-side helper).
pub(crate) fn physical_path(location: DeviceLocation) -> alloc::string::String {
    devnode::physical_path(&location)
}
