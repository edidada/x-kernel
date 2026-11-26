// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Counter-tick and nanosecond conversion for a fixed counter frequency.
//!
//! An integer nanoseconds-per-tick scale drifts for every frequency that does
//! not divide 1 GHz and collapses to zero above 1 GHz. This scale keeps the
//! fractional rate instead, so platform counters such as 24 MHz or 19.2 MHz
//! convert without systematic error.

use int_ratio::Ratio;
use ktime_types::{Frequency, NANOS_PER_SEC};

/// Immutable conversion between counter ticks and nanoseconds.
///
/// Clock reads use an `int_ratio::Ratio` multiply-and-shift factor, whose
/// relative error is below 2^-30 for every accepted frequency; the result
/// wraps once it exceeds `u64` nanoseconds, as in the other ratio-based
/// backends. Nanosecond-to-tick conversions are exact and saturate at
/// `u64::MAX`: an inverse `Ratio` caps its shift at 32 bits, which leaves
/// too few significant bits for counters well below 1 GHz (about 27 at
/// 24 MHz, 3 at 1 Hz).
#[derive(Clone, Copy, Debug)]
pub struct TickScale {
    frequency: Frequency,
    ticks_to_nanos: Ratio,
}

impl TickScale {
    /// Builds the conversion for a counter running at `frequency`.
    ///
    /// # Panics
    ///
    /// Panics when `frequency` is zero or does not fit in `u32` hertz, the
    /// operand range of `int_ratio::Ratio`.
    pub const fn new(frequency: Frequency) -> Self {
        let hz = frequency.as_hz();
        assert!(hz != 0, "timer counter frequency must be non-zero");
        assert!(
            hz <= u32::MAX as u64,
            "timer counter frequency must fit in u32 Hz"
        );
        Self {
            frequency,
            ticks_to_nanos: Ratio::new(NANOS_PER_SEC as u32, hz as u32),
        }
    }

    /// Returns the counter frequency this scale converts for.
    #[inline]
    pub const fn frequency(&self) -> Frequency {
        self.frequency
    }

    /// Converts a counter tick count to nanoseconds, truncating.
    #[inline]
    pub const fn ticks_to_nanos(&self, ticks: u64) -> u64 {
        self.ticks_to_nanos.mul_trunc(ticks)
    }

    /// Converts nanoseconds to whole counter ticks, truncating.
    ///
    /// The result equals `min(floor(nanos * frequency / 1e9), u64::MAX)`.
    #[inline]
    pub const fn nanos_to_ticks(&self, nanos: u64) -> u64 {
        let (whole_sec_ticks, sub_sec_product) = self.split_nanos(nanos);
        whole_sec_ticks.saturating_add(sub_sec_product / NANOS_PER_SEC)
    }

    /// Converts nanoseconds to counter ticks, rounding up.
    ///
    /// The result equals `min(ceil(nanos * frequency / 1e9), u64::MAX)`, so a
    /// timer armed with it never expires before the requested interval.
    #[inline]
    pub const fn nanos_to_ticks_ceil(&self, nanos: u64) -> u64 {
        let (whole_sec_ticks, sub_sec_product) = self.split_nanos(nanos);
        whole_sec_ticks.saturating_add(sub_sec_product.div_ceil(NANOS_PER_SEC))
    }

    /// Splits `nanos * frequency` into the saturated ticks of the whole
    /// seconds and the product of the sub-second remainder, which still has
    /// to be divided by 1e9.
    ///
    /// Scaling the two parts separately keeps every intermediate value in
    /// `u64`: the remainder is below 1e9 and the frequency at most
    /// `u32::MAX`, so their product stays below 2^62.
    #[inline]
    const fn split_nanos(&self, nanos: u64) -> (u64, u64) {
        let hz = self.frequency.as_hz();
        let whole_sec_ticks = (nanos / NANOS_PER_SEC).saturating_mul(hz);
        (whole_sec_ticks, nanos % NANOS_PER_SEC * hz)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/tick_scale.rs");
