// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver time capability.

pub use ktime_types::{MonotonicInstant, TimeSpan};

/// Monotonic clock capability for drivers.
///
/// A host kernel implements this and passes it to its driver framework when a
/// driver needs bounded polling, timeout accounting, or delay loops without
/// depending on the host kernel's clock API directly.
pub trait TimeOp: Sync {
    /// Return the current monotonic time.
    fn monotonic_time(&self) -> MonotonicInstant;

    /// Busy-wait until at least `duration` has elapsed on the monotonic clock.
    ///
    /// Drivers use this to space out register writes that the hardware only
    /// samples at fixed boundaries, such as a display controller latching its
    /// pending state at frame start. The default implementation polls
    /// [`TimeOp::monotonic_time`], which keeps the contract free of host sleep
    /// APIs and safe in any context; a host that runs the delay with a
    /// sleepable context may override it.
    fn delay(&self, duration: TimeSpan) {
        let start = self.monotonic_time();
        while self.monotonic_time().saturating_duration_since(start) < duration {
            core::hint::spin_loop();
        }
    }
}
