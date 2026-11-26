// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{Frequency, NANOS_PER_SEC};
use unittest::{assert, assert_eq, def_test};

use super::TickScale;

const FREQUENCIES_HZ: [u64; 9] = [
    1,
    32_768,
    3_000_000,
    10_000_000,
    19_200_000,
    24_000_000,
    100_000_000,
    1_000_000_000,
    u32::MAX as u64,
];

const SAMPLE_VALUES: [u64; 12] = [
    0,
    1,
    7,
    41,
    999_999_999,
    1_000_000_000,
    1_000_000_001,
    123_456_789_012,
    1 << 40,
    1 << 52,
    1 << 60,
    u64::MAX >> 4,
];

fn exact_ticks_to_nanos(ticks: u64, hz: u64) -> u128 {
    u128::from(ticks) * u128::from(NANOS_PER_SEC) / u128::from(hz)
}

fn saturate(value: u128) -> u64 {
    value.min(u128::from(u64::MAX)) as u64
}

fn exact_nanos_to_ticks(nanos: u64, hz: u64) -> u64 {
    saturate(u128::from(nanos) * u128::from(hz) / u128::from(NANOS_PER_SEC))
}

fn exact_nanos_to_ticks_ceil(nanos: u64, hz: u64) -> u64 {
    saturate((u128::from(nanos) * u128::from(hz)).div_ceil(u128::from(NANOS_PER_SEC)))
}

/// Returns whether a `Ratio` conversion stays within its documented 2^-30
/// relative error, plus one unit of truncation.
fn is_ratio_close(actual: u64, exact: u128) -> bool {
    u128::from(actual).abs_diff(exact) <= (exact >> 30) + 1
}

#[def_test]
fn non_divisor_frequencies_do_not_drift() {
    // An integer scale ran 24 MHz at 41 ns per tick, 1.6% slow; one hour
    // of ticks then read as 59 minutes 2.4 seconds.
    for hz in [3_000_000, 19_200_000, 24_000_000, u32::MAX as u64] {
        let scale = TickScale::new(Frequency::from_hz(hz));
        let one_hour_ticks = hz * 3_600;
        let one_hour_nanos = 3_600 * NANOS_PER_SEC;
        let nanos = scale.ticks_to_nanos(one_hour_ticks);
        assert!(
            is_ratio_close(nanos, u128::from(one_hour_nanos)),
            "{hz} Hz: {nanos} ns"
        );
        assert_eq!(scale.nanos_to_ticks(one_hour_nanos), one_hour_ticks);
        assert_eq!(scale.nanos_to_ticks_ceil(one_hour_nanos), one_hour_ticks);
        assert_eq!(scale.frequency().as_hz(), hz);
    }
}

#[def_test]
fn ticks_to_nanos_matches_wide_reference() {
    for hz in FREQUENCIES_HZ {
        let scale = TickScale::new(Frequency::from_hz(hz));
        for ticks in SAMPLE_VALUES {
            let exact = exact_ticks_to_nanos(ticks, hz);
            // Beyond u64 nanoseconds the documented result wraps.
            if exact <= u128::from(u64::MAX >> 1) {
                let nanos = scale.ticks_to_nanos(ticks);
                assert!(
                    is_ratio_close(nanos, exact),
                    "{hz} Hz, {ticks} ticks: {nanos} ns, exact {exact}"
                );
            }
        }
    }
}

#[def_test]
fn nanos_to_ticks_is_exact_and_saturating() {
    for hz in FREQUENCIES_HZ {
        let scale = TickScale::new(Frequency::from_hz(hz));
        for nanos in SAMPLE_VALUES.into_iter().chain([u64::MAX - 1, u64::MAX]) {
            let floor = scale.nanos_to_ticks(nanos);
            let ceil = scale.nanos_to_ticks_ceil(nanos);
            assert_eq!(floor, exact_nanos_to_ticks(nanos, hz), "{hz} Hz, {nanos} ns");
            assert_eq!(ceil, exact_nanos_to_ticks_ceil(nanos, hz), "{hz} Hz, {nanos} ns");
            assert!(ceil >= floor && ceil - floor <= 1);
        }
    }
    let scale = TickScale::new(Frequency::from_hz(u32::MAX as u64));
    assert_eq!(scale.nanos_to_ticks(u64::MAX), u64::MAX);
    assert_eq!(scale.nanos_to_ticks_ceil(u64::MAX), u64::MAX);
}

#[def_test]
fn ceil_deadline_never_fires_early() {
    for hz in FREQUENCIES_HZ {
        let scale = TickScale::new(Frequency::from_hz(hz));
        for nanos in [1, 41, 42, 999, 1_000_001, 1_234_567_891] {
            let ticks = scale.nanos_to_ticks_ceil(nanos);
            let requested = u128::from(nanos) * u128::from(hz);
            // `ticks` covers the interval and one tick fewer would not.
            assert!(ticks >= 1);
            assert!(u128::from(ticks) * u128::from(NANOS_PER_SEC) >= requested);
            assert!(u128::from(ticks - 1) * u128::from(NANOS_PER_SEC) < requested);
        }
    }
}

#[def_test]
fn round_trip_loses_at_most_one_nanosecond_of_ticks() {
    for hz in FREQUENCIES_HZ {
        let scale = TickScale::new(Frequency::from_hz(hz));
        for ticks in [0, 1, 1_000, hz, hz * 3_600, 1 << 40] {
            if exact_ticks_to_nanos(ticks, hz) > u128::from(u64::MAX >> 1) {
                continue;
            }
            let back = scale.nanos_to_ticks(scale.ticks_to_nanos(ticks));
            // Each truncation drops below one unit: one tick, or one
            // nanosecond's worth of ticks above 1 GHz.
            let tolerance = 1 + hz.div_ceil(NANOS_PER_SEC) + (ticks >> 30);
            assert!(
                back.abs_diff(ticks) <= tolerance,
                "{hz} Hz: {ticks} ticks became {back}"
            );
        }
    }
}
