// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared helpers for the serial driver modules.

use driver_core::{ProbeError, RuntimeError};

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
