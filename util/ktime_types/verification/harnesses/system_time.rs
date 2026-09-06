// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Signed total-nanosecond specifications, including pre-epoch instants.

use super::{arbitrary_timestamp, span};
use crate::{SystemTime, TimeSpan};

const NANOS_IN_SECOND: i128 = 1_000_000_000;
const MIN_NANOS: i128 = i64::MIN as i128 * NANOS_IN_SECOND;
const MAX_NANOS: i128 = i64::MAX as i128 * NANOS_IN_SECOND + NANOS_IN_SECOND - 1;

fn total_nanos(time: SystemTime) -> i128 {
    time.unix_seconds() as i128 * NANOS_IN_SECOND + time.subsec_nanos() as i128
}

#[kani::proof]
#[kani::solver(cvc5)]
fn system_constants_and_seconds() {
    assert!(total_nanos(SystemTime::MIN) == MIN_NANOS);
    assert!(total_nanos(SystemTime::MAX) == MAX_NANOS);
    assert!(total_nanos(SystemTime::UNIX_EPOCH) == 0);
    assert!(SystemTime::default() == SystemTime::UNIX_EPOCH);
    let seconds: i64 = kani::any();
    let time = SystemTime::from_unix_seconds(seconds);
    assert!(time.unix_seconds() == seconds);
    assert!(time.subsec_nanos() == 0);
    assert!(time.unix_nanos() == seconds as i128 * NANOS_IN_SECOND);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn system_parts_have_exact_domain() {
    let seconds: i64 = kani::any();
    let nanoseconds: u32 = kani::any();
    let result = SystemTime::from_unix_parts(seconds, nanoseconds);
    assert!(result.is_some() == ((nanoseconds as i128) < NANOS_IN_SECOND));
    if let Some(time) = result {
        assert!(time.unix_seconds() == seconds);
        assert!(time.subsec_nanos() == nanoseconds);
        assert!(time.unix_nanos() == seconds as i128 * NANOS_IN_SECOND + nanoseconds as i128);
    }
    kani::cover!(nanoseconds == 999_999_999, "last normalized component");
    kani::cover!(nanoseconds == 1_000_000_000, "first rejected component");
    kani::cover!(nanoseconds == u32::MAX, "largest component");
}

#[kani::proof]
#[kani::solver(cvc5)]
fn system_nanos_have_exact_domain_and_floor_normalization() {
    let nanoseconds: i128 = kani::any();
    let result = SystemTime::from_unix_nanos(nanoseconds);
    assert!(result.is_some() == (MIN_NANOS <= nanoseconds && nanoseconds <= MAX_NANOS));
    if let Some(time) = result {
        assert!(total_nanos(time) == nanoseconds);
        assert!((time.subsec_nanos() as i128) < NANOS_IN_SECOND);
        let floor_nanos = time.unix_seconds() as i128 * NANOS_IN_SECOND;
        assert!(floor_nanos <= nanoseconds && nanoseconds < floor_nanos + NANOS_IN_SECOND);
    }
    kani::cover!(nanoseconds == -1, "fraction before epoch");
    kani::cover!(nanoseconds == MIN_NANOS, "minimum accepted");
    kani::cover!(nanoseconds == MAX_NANOS, "maximum accepted");
    kani::cover!(nanoseconds == MIN_NANOS - 1, "first below range");
    kani::cover!(nanoseconds == MAX_NANOS + 1, "first above range");
    kani::cover!(nanoseconds == i128::MIN, "minimum input");
    kani::cover!(nanoseconds == i128::MAX, "maximum input");
}

#[kani::proof]
#[kani::solver(cvc5)]
fn system_nanos_roundtrip_and_order() {
    let left = arbitrary_timestamp();
    let right = arbitrary_timestamp();
    assert!(left.unix_nanos() == total_nanos(left));
    assert!(SystemTime::from_unix_nanos(left.unix_nanos()) == Some(left));
    assert!((left < right) == (total_nanos(left) < total_nanos(right)));
    assert!((left == right) == (total_nanos(left) == total_nanos(right)));
}

#[kani::proof]
#[kani::solver(cvc5)]
fn system_checked_add_matches_signed_total() {
    let time = arbitrary_timestamp();
    let duration = span::arbitrary_span();
    // All representable time/duration totals and their sum fit in i128.
    let sum = total_nanos(time) + span::total_nanos(duration) as i128;
    let result = time.checked_add(duration);
    assert!(result.is_some() == (sum <= MAX_NANOS));
    if let Some(value) = result {
        assert!(total_nanos(value) == sum);
        assert!((value.subsec_nanos() as i128) < NANOS_IN_SECOND);
        assert!(time + duration == value);
        assert!(value.checked_sub(duration) == Some(time));
    }
    kani::cover!(sum == MAX_NANOS, "last valid sum");
    kani::cover!(sum == MAX_NANOS + 1, "first overflowing sum");
    kani::cover!(total_nanos(time) < 0 && sum >= 0, "cross epoch forward");
    kani::cover!(
        time.subsec_nanos() + duration.subsec_nanos() >= 1_000_000_000,
        "carry"
    );
}

#[kani::proof]
#[kani::solver(cvc5)]
fn system_checked_sub_matches_signed_total() {
    let time = arbitrary_timestamp();
    let duration = span::arbitrary_span();
    let difference = total_nanos(time) - span::total_nanos(duration) as i128;
    let result = time.checked_sub(duration);
    assert!(result.is_some() == (difference >= MIN_NANOS));
    if let Some(value) = result {
        assert!(total_nanos(value) == difference);
        assert!((value.subsec_nanos() as i128) < NANOS_IN_SECOND);
        assert!(time - duration == value);
        assert!(value.checked_add(duration) == Some(time));
    }
    kani::cover!(difference == MIN_NANOS, "last valid difference");
    kani::cover!(difference == MIN_NANOS - 1, "first underflowing difference");
    kani::cover!(
        total_nanos(time) >= 0 && difference < 0,
        "cross epoch backward"
    );
    kani::cover!(time.subsec_nanos() < duration.subsec_nanos(), "borrow");
}

#[kani::proof]
#[kani::solver(cvc5)]
fn system_duration_since_reports_exact_magnitude_and_sign() {
    let left = arbitrary_timestamp();
    let right = arbitrary_timestamp();
    let delta = total_nanos(left) - total_nanos(right);
    let result = left.duration_since(right);
    assert!(result.is_ok() == (delta >= 0));
    match result {
        Ok(value) => assert!(span::total_nanos(value) as i128 == delta),
        Err(error) => assert!(span::total_nanos(error.duration()) as i128 == -delta),
    }
    kani::cover!(delta == 0, "equal timestamps");
    kani::cover!(
        delta == MAX_NANOS - MIN_NANOS,
        "largest positive difference"
    );
    kani::cover!(
        delta == MIN_NANOS - MAX_NANOS,
        "largest negative difference"
    );
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn system_add_panics_at_boundary() {
    let _ = SystemTime::MAX + TimeSpan::from_nanos(1);
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn system_sub_panics_at_boundary() {
    let _ = SystemTime::MIN - TimeSpan::from_nanos(1);
}
