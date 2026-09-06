// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A point in the clock domain identified by `C`.
//!
//! Arithmetic with a duration preserves the clock domain:
//!
//! ```
//! use ktime_types::{MonotonicInstant, TimeSpan};
//! let start = MonotonicInstant::ORIGIN;
//! let later = start + TimeSpan::from_nanos(1);
//! assert_eq!(later - start, TimeSpan::from_nanos(1));
//! ```
//!
//! Instants from different clock domains cannot be compared or subtracted:
//!
//! ```compile_fail,E0308
//! use ktime_types::{BoottimeInstant, MonotonicInstant};
//! let _ = MonotonicInstant::ORIGIN < BoottimeInstant::ORIGIN;
//! ```
//!
//! ```compile_fail,E0277
//! use ktime_types::{BoottimeInstant, MonotonicInstant};
//! let _ = MonotonicInstant::ORIGIN - BoottimeInstant::ORIGIN;
//! ```
//!
//! Numeric operations can also be used with an application-defined marker.
//! The constructor and checked arithmetic do not require marker traits:
//!
//! ```
//! use ktime_types::{Instant, TimeSpan};
//! struct ApplicationClock;
//! fn advance<C>(elapsed: TimeSpan, delta: TimeSpan) -> Option<Instant<C>> {
//!     Instant::<C>::from_span_since_origin(elapsed).checked_add(delta)
//! }
//! let time = advance::<ApplicationClock>(TimeSpan::ZERO, TimeSpan::from_secs(1)).unwrap();
//! assert_eq!(time.span_since_origin(), TimeSpan::from_secs(1));
//! ```
