// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Symbolic default-format termination, length bounds, and error propagation.

use core::fmt::{self, Write};

use super::{arbitrary_limits, arbitrary_timestamp};
use crate::{Instant, Monotonic, SystemTime};

struct RejectingWriter;

impl Write for RejectingWriter {
    fn write_str(&mut self, _: &str) -> fmt::Result {
        Err(fmt::Error)
    }
}

struct CountingWriter {
    bytes: usize,
}

struct BudgetWriter {
    remaining_bytes: usize,
    has_rejected_write: bool,
}

impl Write for BudgetWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        assert!(!self.has_rejected_write);
        match self.remaining_bytes.checked_sub(text.len()) {
            Some(remaining_bytes) => {
                self.remaining_bytes = remaining_bytes;
                Ok(())
            }
            None => {
                self.has_rejected_write = true;
                Err(fmt::Error)
            }
        }
    }
}

fn check_budget_error(value: &impl fmt::Debug) {
    let mut writer = BudgetWriter {
        remaining_bytes: kani::any(),
        has_rejected_write: false,
    };
    let result = write!(writer, "{value:?}");
    assert!(result.is_err() == writer.has_rejected_write);
    kani::cover!(result.is_ok(), "budget permits complete output");
    kani::cover!(result.is_err(), "budget rejects output");
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(32)]
fn formatting_span_budget_error() {
    check_budget_error(&super::span::arbitrary_span());
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(32)]
fn formatting_system_budget_error() {
    check_budget_error(&arbitrary_timestamp());
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(32)]
fn formatting_limits_budget_error() {
    check_budget_error(&arbitrary_limits());
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(32)]
fn formatting_instant_budget_error() {
    check_budget_error(&Instant::<Monotonic>::from_span_since_origin(
        super::span::arbitrary_span(),
    ));
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(32)]
fn formatting_error_budget_error() {
    let time = arbitrary_timestamp();
    kani::assume(time > SystemTime::MIN);
    check_budget_error(&SystemTime::MIN.duration_since(time).unwrap_err());
}

impl Write for CountingWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.bytes += text.len();
        Ok(())
    }
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(32)]
fn formatting_span_default_length() {
    let mut writer = CountingWriter { bytes: 0 };
    write!(writer, "{:?}", super::span::arbitrary_span()).unwrap();
    assert!(writer.bytes > 0 && writer.bytes <= 32);
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(32)]
fn formatting_system_default_length() {
    let mut writer = CountingWriter { bytes: 0 };
    write!(writer, "{:?}", arbitrary_timestamp()).unwrap();
    assert!(writer.bytes > 0);
    assert!(writer.bytes <= "SystemTime { seconds: , nanoseconds:  }".len() + 20 + 9);
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(32)]
fn formatting_limits_default_length() {
    let mut writer = CountingWriter { bytes: 0 };
    write!(writer, "{:?}", arbitrary_limits()).unwrap();
    assert!(writer.bytes > 0);
    assert!(
        writer.bytes
            <= "TimestampLimits { granularity_ns: , minimum_seconds: , maximum_seconds:  }".len()
                + 10
                + 20
                + 20
    );
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(32)]
fn formatting_instant_default_length() {
    let value = Instant::<Monotonic>::from_span_since_origin(super::span::arbitrary_span());
    let mut writer = CountingWriter { bytes: 0 };
    write!(writer, "{value:?}").unwrap();
    assert!(writer.bytes > 0 && writer.bytes <= "Instant()".len() + 32);
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(32)]
fn formatting_error_default_length() {
    let time = arbitrary_timestamp();
    kani::assume(time > SystemTime::MIN);
    let error = SystemTime::MIN.duration_since(time).unwrap_err();
    let mut writer = CountingWriter { bytes: 0 };
    write!(writer, "{error:?}").unwrap();
    assert!(writer.bytes > 0 && writer.bytes <= "SystemTimeError { duration:  }".len() + 32);
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(16)]
fn formatting_system_propagates_sink_error() {
    assert!(write!(RejectingWriter, "{:?}", arbitrary_timestamp()).is_err());
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(16)]
fn formatting_limits_propagates_sink_error() {
    assert!(write!(RejectingWriter, "{:?}", arbitrary_limits()).is_err());
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(16)]
fn formatting_instant_propagates_sink_error() {
    let value = Instant::<Monotonic>::from_span_since_origin(super::span::arbitrary_span());
    assert!(write!(RejectingWriter, "{:?}", value).is_err());
}

#[kani::proof]
#[kani::solver(cadical)]
#[kani::unwind(16)]
fn formatting_error_propagates_sink_error() {
    let time = arbitrary_timestamp();
    kani::assume(time > SystemTime::MIN);
    let error = SystemTime::MIN.duration_since(time).unwrap_err();
    assert!(write!(RejectingWriter, "{:?}", error).is_err());
}
