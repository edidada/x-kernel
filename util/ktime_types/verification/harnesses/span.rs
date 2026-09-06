// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TimeSpan contracts expressed as exact total nanoseconds in `u128`.
//!
//! Even the sum of two maximum spans fits in `u128`. This specification can
//! therefore check overflow decisions without overflowing its own arithmetic.

use core::time::Duration;

use crate::TimeSpan;

const NANOS_IN_SECOND: u128 = 1_000_000_000;
pub(super) const MAX_TOTAL_NANOS: u128 = u64::MAX as u128 * NANOS_IN_SECOND + NANOS_IN_SECOND - 1;

pub(super) fn arbitrary_span() -> TimeSpan {
    let seconds: u64 = kani::any();
    let nanoseconds: u32 = kani::any();
    kani::assume((nanoseconds as u128) < NANOS_IN_SECOND);
    TimeSpan::from_core(Duration::new(seconds, nanoseconds))
}

pub(super) fn total_nanos(span: TimeSpan) -> u128 {
    span.as_secs() as u128 * NANOS_IN_SECOND + span.subsec_nanos() as u128
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_core_roundtrip_and_components() {
    let seconds: u64 = kani::any();
    let nanoseconds: u32 = kani::any();
    kani::assume((nanoseconds as u128) < NANOS_IN_SECOND);
    let duration = Duration::new(seconds, nanoseconds);
    let span = TimeSpan::from_core(duration);
    assert!(span.into_core() == duration);
    assert!(TimeSpan::from_core(span.into_core()) == span);
    assert!(span.as_secs() == seconds);
    assert!(span.subsec_nanos() == nanoseconds);
    assert!(span.is_zero() == (seconds == 0 && nanoseconds == 0));
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_constants_and_default() {
    assert!(total_nanos(TimeSpan::ZERO) == 0);
    assert!(TimeSpan::default() == TimeSpan::ZERO);
    assert!(total_nanos(TimeSpan::MAX) == MAX_TOTAL_NANOS);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_unit_constructors_are_exact() {
    let value: u64 = kani::any();
    assert!(total_nanos(TimeSpan::from_secs(value)) == value as u128 * NANOS_IN_SECOND);
    assert!(total_nanos(TimeSpan::from_millis(value)) == value as u128 * 1_000_000);
    assert!(total_nanos(TimeSpan::from_micros(value)) == value as u128 * 1_000);
    assert!(total_nanos(TimeSpan::from_nanos(value)) == value as u128);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_new_normalizes_representable_components() {
    let seconds: u64 = kani::any();
    let nanoseconds: u32 = kani::any();
    let expected = seconds as u128 * NANOS_IN_SECOND + nanoseconds as u128;
    // `new` accepts unnormalized nanoseconds when carrying into seconds fits.
    kani::assume(expected <= MAX_TOTAL_NANOS);
    let span = TimeSpan::new(seconds, nanoseconds);
    assert!(total_nanos(span) == expected);
    assert!((span.subsec_nanos() as u128) < NANOS_IN_SECOND);
    kani::cover!(
        nanoseconds as u128 >= NANOS_IN_SECOND,
        "normalization carry"
    );
    kani::cover!(nanoseconds == u32::MAX, "maximum component");
    kani::cover!(expected == MAX_TOTAL_NANOS, "largest representable span");
}

// Kani's should_panic checks existence, not universality. Use a concrete
// boundary witness rather than claiming all overflowing inputs are proved.
#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn span_new_panics_at_overflow_boundary() {
    let _ = TimeSpan::new(u64::MAX, 1_000_000_000);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_try_from_nanos_has_exact_domain() {
    let nanoseconds: u128 = kani::any();
    let result = TimeSpan::try_from_nanos(nanoseconds);
    assert!(result.is_some() == (nanoseconds <= MAX_TOTAL_NANOS));
    if let Some(span) = result {
        assert!(total_nanos(span) == nanoseconds);
        assert!((span.subsec_nanos() as u128) < NANOS_IN_SECOND);
    }
    kani::cover!(nanoseconds == MAX_TOTAL_NANOS, "last accepted value");
    kani::cover!(nanoseconds == MAX_TOTAL_NANOS + 1, "first rejected value");
    kani::cover!(nanoseconds == u128::MAX, "maximum input");
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_getters_round_down() {
    let span = arbitrary_span();
    let nanoseconds = total_nanos(span);
    assert!(span.as_nanos() == nanoseconds);
    assert!(span.as_micros() == nanoseconds / 1_000);
    assert!(span.as_millis() == nanoseconds / 1_000_000);
    assert!(span.subsec_micros() == span.subsec_nanos() / 1_000);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_nanos_roundtrip() {
    let span = arbitrary_span();
    assert!(TimeSpan::try_from_nanos(span.as_nanos()) == Some(span));
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_u64_conversions_have_exact_boundaries() {
    let span = arbitrary_span();
    let nanoseconds = total_nanos(span);
    let result = span.try_as_nanos_u64();
    assert!(result.is_some() == (nanoseconds <= u64::MAX as u128));
    if let Some(value) = result {
        assert!(value as u128 == nanoseconds);
    }
    assert!(span.as_nanos_u64_saturating() as u128 == nanoseconds.min(u64::MAX as u128));
    kani::cover!(nanoseconds == u64::MAX as u128, "last exact u64 value");
    kani::cover!(
        nanoseconds == u64::MAX as u128 + 1,
        "first saturated u64 value"
    );
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_checked_add_has_exact_domain_and_value() {
    let left = arbitrary_span();
    let right = arbitrary_span();
    let sum = total_nanos(left) + total_nanos(right);
    let result = left.checked_add(right);
    assert!(result.is_some() == (sum <= MAX_TOTAL_NANOS));
    if let Some(span) = result {
        assert!(total_nanos(span) == sum);
        assert!((span.subsec_nanos() as u128) < NANOS_IN_SECOND);
    }
    kani::cover!(sum == MAX_TOTAL_NANOS, "last representable sum");
    kani::cover!(sum == MAX_TOTAL_NANOS + 1, "first overflowing sum");
    kani::cover!(
        left.subsec_nanos() + right.subsec_nanos() >= 1_000_000_000,
        "carry"
    );
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_checked_sub_has_exact_domain_and_value() {
    let left = arbitrary_span();
    let right = arbitrary_span();
    let left_nanos = total_nanos(left);
    let right_nanos = total_nanos(right);
    let result = left.checked_sub(right);
    assert!(result.is_some() == (left_nanos >= right_nanos));
    if let Some(span) = result {
        assert!(total_nanos(span) == left_nanos - right_nanos);
        assert!((span.subsec_nanos() as u128) < NANOS_IN_SECOND);
    }
    kani::cover!(left_nanos == right_nanos, "zero difference");
    kani::cover!(left_nanos + 1 == right_nanos, "underflow by one nanosecond");
    kani::cover!(
        result.is_some() && left.subsec_nanos() < right.subsec_nanos(),
        "borrow"
    );
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_saturating_arithmetic_matches_clamped_totals() {
    let left = arbitrary_span();
    let right = arbitrary_span();
    let left_nanos = total_nanos(left);
    let right_nanos = total_nanos(right);
    assert!(
        total_nanos(left.saturating_add(right)) == (left_nanos + right_nanos).min(MAX_TOTAL_NANOS)
    );
    assert!(total_nanos(left.saturating_sub(right)) == left_nanos.saturating_sub(right_nanos));
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_order_matches_total_nanoseconds() {
    let left = arbitrary_span();
    let right = arbitrary_span();
    assert!((left < right) == (total_nanos(left) < total_nanos(right)));
    assert!((left == right) == (total_nanos(left) == total_nanos(right)));
}

#[kani::proof]
#[kani::solver(cvc5)]
fn span_checked_arithmetic_is_reversible() {
    let left = arbitrary_span();
    let right = arbitrary_span();
    assert!(left.checked_add(right) == right.checked_add(left));
    if let Some(sum) = left.checked_add(right) {
        assert!(sum.checked_sub(right) == Some(left));
    }
    if let Some(difference) = left.checked_sub(right) {
        assert!(difference.checked_add(right) == Some(left));
    }
}
