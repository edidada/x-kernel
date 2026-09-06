// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Universal invalid-input checks, audited by check_ktime_panic_reports.py.
//!
//! `should_panic` alone is insufficient: the runner additionally requires a
//! reachable input domain, an unreachable normal return, and no failures other
//! than the expected panic sites. No checks or implementations are disabled.

use super::{arbitrary_timestamp, span};
use crate::{Boottime, Instant, Monotonic, ProcessCpu, ThreadCpu, TimeSpan, TimestampLimits};

struct ApplicationClock;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct OrderedApplicationClock;

fn add<C>(elapsed: TimeSpan, duration: TimeSpan) {
    let _ = Instant::<C>::from_span_since_origin(elapsed) + duration;
}

fn subtract<C>(elapsed: TimeSpan, duration: TimeSpan) {
    let _ = Instant::<C>::from_span_since_origin(elapsed) - duration;
}

fn difference<C>(left: TimeSpan, right: TimeSpan) {
    let _ =
        Instant::<C>::from_span_since_origin(left) - Instant::<C>::from_span_since_origin(right);
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_span_normalization() {
    let seconds: u64 = kani::any();
    let nanoseconds: u32 = kani::any();
    kani::assume(seconds as u128 * 1_000_000_000 + nanoseconds as u128 > span::MAX_TOTAL_NANOS);
    kani::cover!(true, "panic-domain");
    let _ = TimeSpan::new(seconds, nanoseconds);
    kani::cover!(true, "panic-return");
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_limits_zero_granularity() {
    let minimum: i64 = kani::any();
    let maximum: i64 = kani::any();
    kani::assume(minimum <= maximum);
    kani::cover!(true, "panic-domain");
    let _ = TimestampLimits::new(0, minimum, maximum);
    kani::cover!(true, "panic-return");
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_limits_excess_granularity() {
    let granularity: u32 = kani::any();
    let minimum: i64 = kani::any();
    let maximum: i64 = kani::any();
    kani::assume(granularity > 1_000_000_000);
    kani::assume(minimum <= maximum);
    kani::cover!(true, "panic-domain");
    let _ = TimestampLimits::new(granularity, minimum, maximum);
    kani::cover!(true, "panic-return");
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_limits_reversed_range() {
    let granularity: u32 = kani::any();
    let minimum: i64 = kani::any();
    let maximum: i64 = kani::any();
    // Include simultaneous granularity/range errors in this partition.
    kani::assume(minimum > maximum);
    kani::cover!(true, "panic-domain");
    let _ = TimestampLimits::new(granularity, minimum, maximum);
    kani::cover!(true, "panic-return");
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_instant_add() {
    let elapsed = span::arbitrary_span();
    let duration = span::arbitrary_span();
    kani::assume(span::total_nanos(elapsed) + span::total_nanos(duration) > span::MAX_TOTAL_NANOS);
    kani::cover!(true, "panic-domain");
    match kani::any::<u8>() % 5 {
        0 => add::<Monotonic>(elapsed, duration),
        1 => add::<Boottime>(elapsed, duration),
        2 => add::<ProcessCpu>(elapsed, duration),
        3 => add::<ThreadCpu>(elapsed, duration),
        _ => add::<ApplicationClock>(elapsed, duration),
    }
    kani::cover!(true, "panic-return");
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_instant_sub() {
    let elapsed = span::arbitrary_span();
    let duration = span::arbitrary_span();
    kani::assume(span::total_nanos(elapsed) < span::total_nanos(duration));
    kani::cover!(true, "panic-domain");
    match kani::any::<u8>() % 5 {
        0 => subtract::<Monotonic>(elapsed, duration),
        1 => subtract::<Boottime>(elapsed, duration),
        2 => subtract::<ProcessCpu>(elapsed, duration),
        3 => subtract::<ThreadCpu>(elapsed, duration),
        _ => subtract::<ApplicationClock>(elapsed, duration),
    }
    kani::cover!(true, "panic-return");
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_instant_difference() {
    let left = span::arbitrary_span();
    let right = span::arbitrary_span();
    kani::assume(span::total_nanos(left) < span::total_nanos(right));
    kani::cover!(true, "panic-domain");
    match kani::any::<u8>() % 5 {
        0 => difference::<Monotonic>(left, right),
        1 => difference::<Boottime>(left, right),
        2 => difference::<ProcessCpu>(left, right),
        3 => difference::<ThreadCpu>(left, right),
        _ => difference::<ApplicationClock>(left, right),
    }
    kani::cover!(true, "panic-return");
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_system_add() {
    let time = arbitrary_timestamp();
    let duration = span::arbitrary_span();
    kani::assume(
        time.unix_nanos() + span::total_nanos(duration) as i128
            > i64::MAX as i128 * 1_000_000_000 + 999_999_999,
    );
    kani::cover!(true, "panic-domain");
    let _ = time + duration;
    kani::cover!(true, "panic-return");
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_system_sub() {
    let time = arbitrary_timestamp();
    let duration = span::arbitrary_span();
    kani::assume(
        time.unix_nanos() - (span::total_nanos(duration) as i128)
            < i64::MIN as i128 * 1_000_000_000,
    );
    kani::cover!(true, "panic-domain");
    let _ = time - duration;
    kani::cover!(true, "panic-return");
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_clamp_span() {
    let value = span::arbitrary_span();
    let low = span::arbitrary_span();
    let high = span::arbitrary_span();
    kani::assume(span::total_nanos(low) > span::total_nanos(high));
    kani::cover!(true, "panic-domain");
    let _ = value.clamp(low, high);
    kani::cover!(true, "panic-return");
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_clamp_system() {
    let value = arbitrary_timestamp();
    let low = arbitrary_timestamp();
    let high = arbitrary_timestamp();
    kani::assume(low.unix_nanos() > high.unix_nanos());
    kani::cover!(true, "panic-domain");
    let _ = value.clamp(low, high);
    kani::cover!(true, "panic-return");
}

fn invalid_instant_clamp<C: Copy + Ord>(value: TimeSpan, low: TimeSpan, high: TimeSpan) {
    let _ = Instant::<C>::from_span_since_origin(value).clamp(
        Instant::<C>::from_span_since_origin(low),
        Instant::<C>::from_span_since_origin(high),
    );
}

#[kani::proof]
#[kani::solver(cvc5)]
#[kani::should_panic]
fn panic_contract_clamp_instant() {
    let value = span::arbitrary_span();
    let low = span::arbitrary_span();
    let high = span::arbitrary_span();
    kani::assume(span::total_nanos(low) > span::total_nanos(high));
    kani::cover!(true, "panic-domain");
    match kani::any::<u8>() % 5 {
        0 => invalid_instant_clamp::<Monotonic>(value, low, high),
        1 => invalid_instant_clamp::<Boottime>(value, low, high),
        2 => invalid_instant_clamp::<ProcessCpu>(value, low, high),
        3 => invalid_instant_clamp::<ThreadCpu>(value, low, high),
        _ => invalid_instant_clamp::<OrderedApplicationClock>(value, low, high),
    }
    kani::cover!(true, "panic-return");
}
