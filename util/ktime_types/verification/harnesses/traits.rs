// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Value-trait laws and their correspondence to observable numeric values.

use core::{
    cmp::Ordering,
    hash::{Hash, Hasher},
};

use super::{arbitrary_limits, arbitrary_timestamp, span};
use crate::{Boottime, Instant, Monotonic, ProcessCpu, ThreadCpu};

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct CustomClock;

#[kani::proof]
#[kani::solver(cvc5)]
fn custom_clock_constructors() {
    super::instant::constructors::<CustomClock>();
}
#[kani::proof]
#[kani::solver(cvc5)]
fn custom_clock_addition() {
    super::instant::addition::<CustomClock>();
}
#[kani::proof]
#[kani::solver(cvc5)]
fn custom_clock_subtraction() {
    super::instant::subtraction::<CustomClock>();
}
#[kani::proof]
#[kani::solver(cvc5)]
fn custom_clock_differences() {
    super::instant::differences::<CustomClock>();
}

fn value_traits<T: Copy + Ord>(left: T, right: T, expected: Ordering) {
    let copied = left;
    // Explicit calls exercise Clone and clone_from rather than only Copy.
    #[allow(clippy::clone_on_copy)]
    let cloned = left.clone();
    let mut replaced = right;
    replaced.clone_from(&left);
    assert!(copied == left && cloned == left && replaced == left);
    assert!(left.cmp(&right) == expected);
    assert!(left.partial_cmp(&right) == Some(expected));
    assert!((left == right) == (expected == Ordering::Equal));
    assert!((left != right) == (expected != Ordering::Equal));
    assert!((left < right) == (expected == Ordering::Less));
    assert!((left <= right) == (expected != Ordering::Greater));
    assert!((left > right) == (expected == Ordering::Greater));
    assert!((left >= right) == (expected != Ordering::Less));
    assert!(
        left.min(right)
            == if expected == Ordering::Greater {
                right
            } else {
                left
            }
    );
    assert!(
        left.max(right)
            == if expected == Ordering::Less {
                right
            } else {
                left
            }
    );
    let low = left.min(right);
    let high = left.max(right);
    assert!(left.clamp(low, high) == left);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_span_order_and_copy() {
    let left = span::arbitrary_span();
    let right = span::arbitrary_span();
    value_traits(
        left,
        right,
        span::total_nanos(left).cmp(&span::total_nanos(right)),
    );
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_system_order_and_copy() {
    let left = arbitrary_timestamp();
    let right = arbitrary_timestamp();
    value_traits(left, right, left.unix_nanos().cmp(&right.unix_nanos()));
}

fn instant_traits<C: Copy + Ord>() {
    let left = span::arbitrary_span();
    let right = span::arbitrary_span();
    value_traits(
        Instant::<C>::from_span_since_origin(left),
        Instant::<C>::from_span_since_origin(right),
        span::total_nanos(left).cmp(&span::total_nanos(right)),
    );
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_instant_monotonic() {
    instant_traits::<Monotonic>();
}
#[kani::proof]
#[kani::solver(cvc5)]
fn traits_instant_boottime() {
    instant_traits::<Boottime>();
}
#[kani::proof]
#[kani::solver(cvc5)]
fn traits_instant_process_cpu() {
    instant_traits::<ProcessCpu>();
}
#[kani::proof]
#[kani::solver(cvc5)]
fn traits_instant_thread_cpu() {
    instant_traits::<ThreadCpu>();
}
#[kani::proof]
#[kani::solver(cvc5)]
fn traits_instant_custom_clock() {
    instant_traits::<CustomClock>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_limits_equality_and_copy() {
    let left = arbitrary_limits();
    let right = arbitrary_limits();
    let same = left.granularity_ns() == right.granularity_ns()
        && left.minimum_seconds() == right.minimum_seconds()
        && left.maximum_seconds() == right.maximum_seconds();
    assert!((left == right) == same);
    assert!((left != right) == !same);
    #[allow(clippy::clone_on_copy)]
    let cloned = left.clone();
    let copied = left;
    let mut replaced = right;
    replaced.clone_from(&left);
    assert!(cloned == left && copied == left && replaced == left);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_error_equality_and_copy() {
    let earlier = crate::SystemTime::MIN;
    let left_time = arbitrary_timestamp();
    let right_time = arbitrary_timestamp();
    kani::assume(left_time > earlier && right_time > earlier);
    let left = earlier.duration_since(left_time).unwrap_err();
    let right = earlier.duration_since(right_time).unwrap_err();
    assert!((left == right) == (left.duration() == right.duration()));
    assert!((left != right) == (left.duration() != right.duration()));
    #[allow(clippy::clone_on_copy)]
    let cloned = left.clone();
    let copied = left;
    let mut replaced = right;
    replaced.clone_from(&left);
    assert!(cloned == left && copied == left && replaced == left);
}

// Record the primitive Hash calls used by these value types, without hash
// collisions. Unsupported writes fail, so an implementation change cannot
// silently extend the observed stream beyond this proof's scope.
#[derive(Default, Eq, PartialEq)]
struct HashTrace {
    events: [(u8, u64); 2],
    count: usize,
}

impl HashTrace {
    fn record(&mut self, kind: u8, value: u64) {
        assert!(self.count < self.events.len());
        self.events[self.count] = (kind, value);
        self.count += 1;
    }
}

impl Hasher for HashTrace {
    fn finish(&self) -> u64 {
        panic!("Hash must not finalize the caller's hasher");
    }

    fn write(&mut self, _: &[u8]) {
        panic!("unexpected raw-byte Hash write");
    }

    fn write_u64(&mut self, value: u64) {
        self.record(1, value);
    }

    fn write_u32(&mut self, value: u32) {
        self.record(2, value as u64);
    }

    fn write_i64(&mut self, value: i64) {
        self.record(3, value as u64);
    }
}

fn trace<T: Hash>(value: T) -> HashTrace {
    let mut trace = HashTrace::default();
    value.hash(&mut trace);
    trace
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_span_hash_is_exact_component_stream() {
    let value = span::arbitrary_span();
    let expected = HashTrace {
        events: [(1, value.as_secs()), (2, value.subsec_nanos() as u64)],
        count: 2,
    };
    assert!(trace(value) == expected);
    assert!(trace(value.into_core()) == expected);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_system_hash_is_exact_component_stream() {
    let value = arbitrary_timestamp();
    let expected = HashTrace {
        events: [
            (3, value.unix_seconds() as u64),
            (2, value.subsec_nanos() as u64),
        ],
        count: 2,
    };
    assert!(trace(value) == expected);
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_instant_hash_all_domains() {
    let value = span::arbitrary_span();
    let expected = trace(value);
    assert!(trace(Instant::<Monotonic>::from_span_since_origin(value)) == expected);
    assert!(trace(Instant::<Boottime>::from_span_since_origin(value)) == expected);
    assert!(trace(Instant::<ProcessCpu>::from_span_since_origin(value)) == expected);
    assert!(trace(Instant::<ThreadCpu>::from_span_since_origin(value)) == expected);
    assert!(trace(Instant::<CustomClock>::from_span_since_origin(value)) == expected);
}

fn clamp_value<T: Copy + Ord>(value: T, low: T, high: T, number: impl Fn(T) -> i128) {
    let value_nanos = number(value);
    let low_nanos = number(low);
    let high_nanos = number(high);
    kani::assume(low_nanos <= high_nanos);
    let expected = if value_nanos < low_nanos {
        low_nanos
    } else if value_nanos > high_nanos {
        high_nanos
    } else {
        value_nanos
    };
    assert!(number(value.clamp(low, high)) == expected);
    kani::cover!(value_nanos < low_nanos, "below lower bound");
    kani::cover!(value_nanos > high_nanos, "above upper bound");
    kani::cover!(
        low_nanos < value_nanos && value_nanos < high_nanos,
        "strict interior"
    );
    kani::cover!(value_nanos == low_nanos, "lower endpoint");
    kani::cover!(value_nanos == high_nanos, "upper endpoint");
    kani::cover!(low_nanos == high_nanos, "singleton interval");
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_clamp_span() {
    clamp_value(
        span::arbitrary_span(),
        span::arbitrary_span(),
        span::arbitrary_span(),
        |value| span::total_nanos(value) as i128,
    );
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_clamp_system() {
    clamp_value(
        arbitrary_timestamp(),
        arbitrary_timestamp(),
        arbitrary_timestamp(),
        |value| value.unix_nanos(),
    );
}

fn instant_clamp<C: Copy + Ord>() {
    clamp_value(
        Instant::<C>::from_span_since_origin(span::arbitrary_span()),
        Instant::<C>::from_span_since_origin(span::arbitrary_span()),
        Instant::<C>::from_span_since_origin(span::arbitrary_span()),
        |value| span::total_nanos(value.span_since_origin()) as i128,
    );
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_clamp_instant_monotonic() {
    instant_clamp::<Monotonic>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_clamp_instant_boottime() {
    instant_clamp::<Boottime>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_clamp_instant_process_cpu() {
    instant_clamp::<ProcessCpu>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_clamp_instant_thread_cpu() {
    instant_clamp::<ThreadCpu>();
}

#[kani::proof]
#[kani::solver(cvc5)]
fn traits_clamp_instant_custom_clock() {
    instant_clamp::<CustomClock>();
}
