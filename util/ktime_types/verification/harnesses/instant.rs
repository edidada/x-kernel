// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Contracts instantiated for all four exported clock domains.

use super::span;
use crate::{Boottime, Instant, Monotonic, ProcessCpu, ThreadCpu, TimeSpan};

pub(super) fn constructors<C: Copy + Ord>() {
    let elapsed = span::arbitrary_span();
    let instant = Instant::<C>::from_span_since_origin(elapsed);
    assert!(instant.span_since_origin() == elapsed);
    assert!(Instant::<C>::ORIGIN.span_since_origin() == TimeSpan::ZERO);
    assert!(
        instant.as_nanos_u64_saturating() as u128
            == span::total_nanos(elapsed).min(u64::MAX as u128)
    );
}

pub(super) fn addition<C: Copy + Ord>() {
    let elapsed = span::arbitrary_span();
    let duration = span::arbitrary_span();
    let instant = Instant::<C>::from_span_since_origin(elapsed);
    let origin_nanos = span::total_nanos(elapsed);
    let duration_nanos = span::total_nanos(duration);
    let sum = origin_nanos + duration_nanos;
    let added = instant.checked_add(duration);
    assert!(added.is_some() == (sum <= span::MAX_TOTAL_NANOS));
    if let Some(value) = added {
        assert!(span::total_nanos(value.span_since_origin()) == sum);
        assert!(instant + duration == value);
        assert!(value.checked_sub(duration) == Some(instant));
    }
    kani::cover!(
        sum == span::MAX_TOTAL_NANOS + 1,
        "addition overflow boundary"
    );
}

pub(super) fn subtraction<C: Copy + Ord>() {
    let elapsed = span::arbitrary_span();
    let duration = span::arbitrary_span();
    let instant = Instant::<C>::from_span_since_origin(elapsed);
    let origin_nanos = span::total_nanos(elapsed);
    let duration_nanos = span::total_nanos(duration);
    let subtracted = instant.checked_sub(duration);
    assert!(subtracted.is_some() == (origin_nanos >= duration_nanos));
    if let Some(value) = subtracted {
        assert!(span::total_nanos(value.span_since_origin()) == origin_nanos - duration_nanos);
        assert!(instant - duration == value);
        assert!(value.checked_add(duration) == Some(instant));
    }
    kani::cover!(
        origin_nanos + 1 == duration_nanos,
        "before origin by one nanosecond"
    );
}

pub(super) fn differences<C: Copy + Ord>() {
    let left_span = span::arbitrary_span();
    let right_span = span::arbitrary_span();
    let left = Instant::<C>::from_span_since_origin(left_span);
    let right = Instant::<C>::from_span_since_origin(right_span);
    let left_nanos = span::total_nanos(left_span);
    let right_nanos = span::total_nanos(right_span);
    assert!((left < right) == (left_nanos < right_nanos));
    assert!((left == right) == (left_nanos == right_nanos));
    let difference = left.checked_duration_since(right);
    assert!(difference.is_some() == (left_nanos >= right_nanos));
    if let Some(value) = difference {
        assert!(span::total_nanos(value) == left_nanos - right_nanos);
        assert!(left - right == value);
    }
    assert!(
        span::total_nanos(left.saturating_duration_since(right))
            == left_nanos.saturating_sub(right_nanos)
    );
    kani::cover!(left_nanos == right_nanos, "equal instants");
    kani::cover!(left_nanos < right_nanos, "reversed instants");
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_constructors_all_domains() {
    constructors::<Monotonic>();
    constructors::<Boottime>();
    constructors::<ProcessCpu>();
    constructors::<ThreadCpu>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_addition_monotonic() {
    addition::<Monotonic>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_subtraction_monotonic() {
    subtraction::<Monotonic>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_addition_boottime() {
    addition::<Boottime>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_subtraction_boottime() {
    subtraction::<Boottime>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_addition_process_cpu() {
    addition::<ProcessCpu>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_subtraction_process_cpu() {
    subtraction::<ProcessCpu>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_addition_thread_cpu() {
    addition::<ThreadCpu>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_subtraction_thread_cpu() {
    subtraction::<ThreadCpu>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_differences_monotonic() {
    differences::<Monotonic>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_differences_boottime() {
    differences::<Boottime>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_differences_process_cpu() {
    differences::<ProcessCpu>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn instant_differences_thread_cpu() {
    differences::<ThreadCpu>();
}

// Concrete panic witnesses; should_panic does not prove all invalid inputs.
#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn instant_add_panics_at_boundary() {
    let _ = Instant::<Monotonic>::from_span_since_origin(TimeSpan::MAX) + TimeSpan::from_nanos(1);
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn instant_sub_panics_before_origin() {
    let _ = Instant::<Monotonic>::ORIGIN - TimeSpan::from_nanos(1);
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn instant_difference_panics_for_reversed_order() {
    let _ = Instant::<Monotonic>::ORIGIN
        - Instant::<Monotonic>::from_span_since_origin(TimeSpan::from_nanos(1));
}
