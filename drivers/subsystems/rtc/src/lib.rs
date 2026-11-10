// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real-time clock (RTC) device class.
//!
//! Drivers implement [`RtcDevice`] and publish into [`RTC`] through the
//! framework's one class-publication door (`dev.publish(&RTC, …)`), exactly
//! like the char, block, display, input, net, vsock and hwrng classes. The
//! class is the Linux `rtc_class` analogue: one registry, one trait, one
//! consumer surface.
//!
//! The class has two halves, split per the L0-b rule:
//!
//! * the **contract** — [`RtcDevice`] and the published payload type — is
//!   host-free, so drivers can be exercised against mock services;
//! * the **service** — [`hctosys`] — is the subsystem-owned implementation
//!   the kernel runs after probe time to seed the wall clock (Linux
//!   `rtc_hctosys()`). It touches the kernel time subsystem, which is why it
//!   lives here and not in the contract.
//!
//! Timing: a modelled RTC is discovered and probed with every other device
//! (initcall level L5 in `driver-init`), so the wall clock is seeded after
//! the probe storm — not from a boot-stage per-platform call. This matches
//! Linux, where `rtc_hctosys` is a late initcall; [`ktime::initialize_realtime`]
//! is idempotent, so a platform that seeds earlier keeps its sample.

#![no_std]

extern crate alloc;

pub mod class;
pub use class::*;
pub use kerrno::{KError, KResult};
use ktime_types::SystemTime;

/// A persistent real-time clock, implemented by a concrete device driver.
///
/// The trait is intentionally minimal: read the wall clock. Alarm, wakeup
/// and periodic-interrupt surfaces are added per driver when a consumer
/// needs them, not pre-declared here.
pub trait RtcDevice: Send + Sync {
    /// Sample the current wall-clock time from the device.
    ///
    /// Returns [`KError::Io`] when the device is present but cannot supply a
    /// trustworthy sample (faulting registers, an unstable year boundary),
    /// and [`KError::Unsupported`] when the driver cannot reach its hardware
    /// state. The call does not sleep; it may run in boot context.
    fn read_time(&self) -> KResult<SystemTime>;
}

/// Seeds the kernel wall clock from the first RTC that yields a sample.
///
/// Called once by the composition root after the probe storm. Returns the
/// accepted sample, or `None` when the class is empty or every device failed
/// — a missing RTC is not a boot failure, so the caller only logs it.
///
/// Later calls are harmless: [`ktime::initialize_realtime`] keeps the first
/// established correlation.
pub fn hctosys() -> Option<SystemTime> {
    for device in RTC.devices() {
        match device.read_time() {
            Ok(sample) => {
                ktime::initialize_realtime(sample);
                log::info!(
                    "rtc: wall clock seeded at unix {} s by `{}`",
                    sample.unix_seconds(),
                    device.driver_name().unwrap_or("<unknown>")
                );
                return Some(sample);
            }
            Err(err) => log::warn!("rtc: device read failed: {err:?}"),
        }
    }
    None
}
