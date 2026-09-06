// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host regression tests for formatting; these are not symbolic proofs.

use std::fmt::{self, Debug, Write};

use ktime_types::{MonotonicInstant, SystemTime, TimeSpan, TimestampLimits};

const SECONDS: &[u64] = &[0, 1, 9, 10, 99, 100, 999, 1000, u32::MAX as u64, u64::MAX];
const NANOS: &[u32] = &[
    0,
    1,
    9,
    10,
    999,
    1000,
    1001,
    999_999,
    1_000_000,
    123_456_789,
    999_999_999,
];

#[test]
fn span_preserves_duration_formatting_options() {
    for &seconds in SECONDS {
        for &nanos in NANOS {
            let span = TimeSpan::new(seconds, nanos);
            let duration = std::time::Duration::new(seconds, nanos);
            assert_eq!(format!("{span:?}"), format!("{duration:?}"));
            assert_eq!(format!("{span:#?}"), format!("{duration:#?}"));
            for precision in [0, 1, 3, 9, 12] {
                for width in [0, 1, 16, 32, 64] {
                    assert_eq!(
                        format!("{span:width$.precision$?}"),
                        format!("{duration:width$.precision$?}")
                    );
                }
            }
        }
    }
}

#[test]
fn instant_format_has_clock_independent_wrapper() {
    for &seconds in SECONDS {
        for &nanos in NANOS {
            let elapsed = TimeSpan::new(seconds, nanos);
            let instant = MonotonicInstant::from_span_since_origin(elapsed);
            assert_eq!(
                format!("{instant:?}"),
                format!("Instant({:?})", elapsed.into_core())
            );
            assert_eq!(
                format!("{instant:#?}"),
                format!("Instant(\n    {:?},\n)", elapsed.into_core())
            );
        }
    }
}

#[test]
fn signed_time_format_preserves_fields() {
    for seconds in [i64::MIN, -1_000_000, -1, 0, 1, 1_000_000, i64::MAX] {
        for &nanoseconds in NANOS {
            let time = SystemTime::from_unix_parts(seconds, nanoseconds).unwrap();
            assert_eq!(
                format!("{time:?}"),
                format!("SystemTime {{ seconds: {seconds}, nanoseconds: {nanoseconds} }}")
            );
            assert_eq!(
                format!("{time:#?}"),
                format!(
                    "SystemTime {{\n    seconds: {seconds},\n    nanoseconds: {nanoseconds},\n}}"
                )
            );
        }
    }
}

#[test]
fn limits_format_preserves_capabilities() {
    for granularity in [1, 10, 999, 1_000_000, 1_000_000_000] {
        for (minimum, maximum) in [(i64::MIN, i64::MAX), (-1, 1), (0, 0)] {
            let limits = TimestampLimits::new(granularity, minimum, maximum);
            assert_eq!(
                format!("{limits:?}"),
                format!(
                    "TimestampLimits {{ granularity_ns: {granularity}, minimum_seconds: \
                     {minimum}, maximum_seconds: {maximum} }}"
                )
            );
            assert_eq!(
                format!("{limits:#?}"),
                format!(
                    "TimestampLimits {{\n    granularity_ns: {granularity},\n    minimum_seconds: \
                     {minimum},\n    maximum_seconds: {maximum},\n}}"
                )
            );
        }
    }
}

#[test]
fn error_format_preserves_positive_magnitude() {
    for later in [
        SystemTime::from_unix_nanos(-1).unwrap(),
        SystemTime::UNIX_EPOCH,
        SystemTime::MAX,
    ] {
        let error = SystemTime::MIN.duration_since(later).unwrap_err();
        let magnitude = error.duration().into_core();
        assert_eq!(
            format!("{error:?}"),
            format!("SystemTimeError {{ duration: {magnitude:?} }}")
        );
        assert_eq!(
            format!("{error:#?}"),
            format!("SystemTimeError {{\n    duration: {magnitude:?},\n}}")
        );
    }
}

struct LimitedWriter {
    remaining: usize,
    has_failed: bool,
}

impl Write for LimitedWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        assert!(
            !self.has_failed,
            "formatter continued writing after failure"
        );
        if text.len() > self.remaining {
            self.has_failed = true;
            return Err(fmt::Error);
        }
        self.remaining -= text.len();
        Ok(())
    }
}

fn check_every_sink_budget(value: impl Debug) {
    for pretty in [false, true] {
        let text = if pretty {
            format!("{value:#?}")
        } else {
            format!("{value:?}")
        };
        for capacity in 0..=text.len() + 1 {
            let mut sink = LimitedWriter {
                remaining: capacity,
                has_failed: false,
            };
            let result = if pretty {
                write!(sink, "{value:#?}")
            } else {
                write!(sink, "{value:?}")
            };
            assert_eq!(result.is_ok(), capacity >= text.len());
            assert_eq!(sink.has_failed, result.is_err());
        }
    }
}

#[test]
fn all_value_formats_propagate_sink_failures() {
    for span in [
        TimeSpan::ZERO,
        TimeSpan::from_nanos(1),
        TimeSpan::new(1, 123_456_789),
        TimeSpan::MAX,
    ] {
        check_every_sink_budget(span);
        check_every_sink_budget(MonotonicInstant::from_span_since_origin(span));
    }
    for time in [SystemTime::MIN, SystemTime::UNIX_EPOCH, SystemTime::MAX] {
        check_every_sink_budget(time);
    }
    check_every_sink_budget(SystemTime::MIN.duration_since(SystemTime::MAX).unwrap_err());
    check_every_sink_budget(TimestampLimits::NANOSECOND);
    check_every_sink_budget(TimestampLimits::new(1_000, -1, 1));
}
