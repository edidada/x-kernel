// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Proofs over production time values with all legal input values.
//!
//! Input assumptions encode each property's domain. No implementation is
//! stubbed. Numeric harnesses need no explicit unwind bound; formatting
//! harnesses retain unwind checks with their documented bound. Universal panic
//! contracts additionally require the runner's normal-return reachability audit.

use crate::{NANOS_PER_SEC, SystemTime, TimestampLimits};

mod formatting;
mod instant;
mod panic_contracts;
mod span;
mod system_time;
mod traits;

fn arbitrary_limits() -> TimestampLimits {
    let granularity_ns: u32 = kani::any();
    let minimum_seconds: i64 = kani::any();
    let maximum_seconds: i64 = kani::any();
    kani::assume(granularity_ns > 0 && granularity_ns <= NANOS_PER_SEC as u32);
    kani::assume(minimum_seconds <= maximum_seconds);
    TimestampLimits::new(granularity_ns, minimum_seconds, maximum_seconds)
}

fn arbitrary_timestamp() -> SystemTime {
    let seconds: i64 = kani::any();
    let nanoseconds: u32 = kani::any();
    kani::assume(nanoseconds < NANOS_PER_SEC as u32);
    SystemTime::from_unix_parts(seconds, nanoseconds).unwrap()
}

#[kani::proof]
#[kani::solver(cvc5)]
fn truncate_preserves_range_and_normalization() {
    let limits = arbitrary_limits();
    let timestamp = arbitrary_timestamp();
    let result = limits.truncate(timestamp);
    assert!(result >= SystemTime::from_unix_seconds(limits.minimum_seconds()));
    assert!(result <= SystemTime::from_unix_seconds(limits.maximum_seconds()));
    assert!(result.subsec_nanos() < NANOS_PER_SEC as u32);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn truncate_respects_granularity() {
    let limits = arbitrary_limits();
    let result = limits.truncate(arbitrary_timestamp());
    assert!(result.subsec_nanos() % limits.granularity_ns() == 0);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn truncate_is_idempotent() {
    let limits = arbitrary_limits();
    let result = limits.truncate(arbitrary_timestamp());
    assert!(limits.truncate(result) == result);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn truncate_is_monotonic() {
    let limits = arbitrary_limits();
    let earlier = arbitrary_timestamp();
    let later = arbitrary_timestamp();
    kani::assume(earlier <= later);
    assert!(limits.truncate(earlier) <= limits.truncate(later));
}

#[kani::proof]
#[kani::solver(cvc5)]
fn truncate_rounds_interior_down_by_less_than_one_granule() {
    let limits = arbitrary_limits();
    let timestamp = arbitrary_timestamp();
    kani::assume(timestamp.unix_seconds() > limits.minimum_seconds());
    kani::assume(timestamp.unix_seconds() < limits.maximum_seconds());
    let result = limits.truncate(timestamp);
    assert!(result.unix_seconds() == timestamp.unix_seconds());
    assert!(result <= timestamp);
    assert!(timestamp.subsec_nanos() - result.subsec_nanos() < limits.granularity_ns());
}

#[kani::proof]
#[kani::solver(cvc5)]
fn truncate_input_classes_are_reachable() {
    let limits = arbitrary_limits();
    let timestamp = arbitrary_timestamp();
    let result = limits.truncate(timestamp);
    let seconds = timestamp.unix_seconds();
    kani::cover!(seconds < limits.minimum_seconds(), "below minimum");
    kani::cover!(seconds == limits.minimum_seconds(), "at minimum");
    kani::cover!(
        seconds > limits.minimum_seconds() && seconds < limits.maximum_seconds(),
        "interior"
    );
    kani::cover!(seconds == limits.maximum_seconds(), "at maximum");
    kani::cover!(seconds > limits.maximum_seconds(), "above maximum");
    kani::cover!(
        limits.minimum_seconds() == limits.maximum_seconds(),
        "singleton range"
    );
    kani::cover!(seconds == i64::MIN, "minimum signed seconds");
    kani::cover!(seconds == i64::MAX, "maximum signed seconds");
    kani::cover!(limits.granularity_ns() == 1, "nanosecond precision");
    kani::cover!(
        limits.granularity_ns() == NANOS_PER_SEC as u32,
        "second precision"
    );
    kani::cover!(result.subsec_nanos() != 0, "nonzero rounded fraction");
}

#[kani::proof]
#[kani::solver(cvc5)]
fn timestamp_limits_construction_preserves_components() {
    let granularity_ns: u32 = kani::any();
    let minimum_seconds: i64 = kani::any();
    let maximum_seconds: i64 = kani::any();
    kani::assume(granularity_ns > 0 && granularity_ns <= 1_000_000_000);
    kani::assume(minimum_seconds <= maximum_seconds);
    let limits = TimestampLimits::new(granularity_ns, minimum_seconds, maximum_seconds);
    assert!(limits.granularity_ns() == granularity_ns);
    assert!(limits.minimum_seconds() == minimum_seconds);
    assert!(limits.maximum_seconds() == maximum_seconds);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn timestamp_limits_constants_and_units() {
    assert!(crate::MILLIS_PER_SEC == 1_000);
    assert!(crate::MICROS_PER_SEC == 1_000_000);
    assert!(crate::NANOS_PER_SEC == 1_000_000_000);
    assert!(crate::NANOS_PER_MICROS == 1_000);
    assert!(crate::NANOS_PER_MILLIS == 1_000_000);
    let nanosecond = TimestampLimits::NANOSECOND;
    assert!(nanosecond.granularity_ns() == 1);
    assert!(nanosecond.minimum_seconds() == i64::MIN);
    assert!(nanosecond.maximum_seconds() == i64::MAX);
    let second = TimestampLimits::SECOND;
    assert!(second.granularity_ns() == 1_000_000_000);
    assert!(second.minimum_seconds() == i64::MIN);
    assert!(second.maximum_seconds() == i64::MAX);
    assert!(TimestampLimits::default() == second);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn truncate_clamps_to_exact_endpoints() {
    let limits = arbitrary_limits();
    let timestamp = arbitrary_timestamp();
    let result = limits.truncate(timestamp);
    if timestamp.unix_seconds() <= limits.minimum_seconds() {
        assert!(result == SystemTime::from_unix_seconds(limits.minimum_seconds()));
    }
    if timestamp.unix_seconds() >= limits.maximum_seconds() {
        assert!(result == SystemTime::from_unix_seconds(limits.maximum_seconds()));
    }
}

// Concrete witnesses cover each documented constructor panic category.
#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn timestamp_limits_panics_for_zero_granularity() {
    let _ = TimestampLimits::new(0, 0, 1);
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn timestamp_limits_panics_for_excess_granularity() {
    let _ = TimestampLimits::new(1_000_000_001, 0, 1);
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn timestamp_limits_panics_for_reversed_range() {
    let _ = TimestampLimits::new(1, 1, 0);
}
