// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared helpers for the RTC driver modules: error mapping and epoch
//! conversion.

use driver_base::{ProbeError, RuntimeError};
use kerrno::KError;
#[cfg(any(feature = "pl031", feature = "goldfish", feature = "cmos"))]
use kerrno::KResult;
#[cfg(any(feature = "pl031", feature = "goldfish", feature = "cmos"))]
use ktime_types::SystemTime;

/// Convert an unsigned unix-seconds sample into a validated [`SystemTime`].
///
/// Used by the PL031, Goldfish and CMOS backends; the LS7A decoder builds
/// its timestamp field by field from the packed TOY word instead.
#[cfg(any(feature = "pl031", feature = "goldfish", feature = "cmos"))]
pub(crate) fn system_time_from_unsigned_seconds(seconds: u64) -> KResult<SystemTime> {
    i64::try_from(seconds)
        .map(SystemTime::from_unix_seconds)
        .map_err(|_| KError::InvalidInput)
}

/// Map a class-publication error into a probe result. The class layer's
/// vocabulary is the former `DriverError` set; any other kerrno code is
/// foreign here and treated as an I/O failure.
pub(crate) fn map_driver_error(err: KError) -> ProbeError {
    let error = match err {
        KError::InvalidInput | KError::ReadOnlyFilesystem => RuntimeError::Invalid,
        KError::NoMemory => RuntimeError::NoMemory,
        KError::ResourceBusy | KError::AlreadyExists => RuntimeError::Busy,
        KError::BadState => RuntimeError::BadState,
        KError::Io | KError::WouldBlock => RuntimeError::Io,
        KError::Unsupported => RuntimeError::Unsupported,
        _ => RuntimeError::Io,
    };
    ProbeError::Fatal(error)
}
