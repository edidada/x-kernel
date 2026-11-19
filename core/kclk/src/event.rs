// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Rate-change notification events and subscriber type.
//!
//! A driver that depends on a clock's rate staying fixed (a display scanning
//! out at a pixel clock, a UART with a cached divisor) registers a
//! [`ClkNotifier`] on the clock. When the framework changes the rate, it
//! delivers [`ClkEvent`] callbacks; a `PreRateChange` subscriber may veto the
//! change by returning [`NotifierResult::Stop`].
//!
//! The notification scheme mirrors the Linux `clk_notifier` chain. Callbacks
//! run while the global clock lock (a sleepable mutex) is held, so they may
//! sleep but must be quick and must not re-enter the clock framework —
//! re-entrancy panics (see the crate design doc).

use alloc::sync::Arc;

use crate::Rate;

/// Rate-change event delivered to clock subscribers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClkEvent {
    /// The rate is about to change from `old` to `new`. A subscriber may veto
    /// by returning [`NotifierResult::Stop`].
    PreRateChange {
        /// Current rate, before the change.
        old: Rate,
        /// Proposed new rate.
        new: Rate,
    },
    /// The rate has changed from `old` to `new`.
    PostRateChange {
        /// Previous rate.
        old: Rate,
        /// New rate now in effect.
        new: Rate,
    },
    /// A rate change was vetoed or failed; the rate remains `old`.
    RateChangeAborted {
        /// Rate that remains in effect.
        old: Rate,
        /// Rate that was attempted.
        attempted: Rate,
    },
}

/// Outcome returned by a rate-change subscriber.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NotifierResult {
    /// Allow the change to proceed.
    #[default]
    Proceed,
    /// Veto an upcoming rate change (only meaningful for
    /// [`ClkEvent::PreRateChange`]).
    Stop,
}

/// A clock rate-change subscriber callback.
///
/// Implemented as a trait object so subscribers can carry arbitrary captured
/// state, stored behind an [`Arc`] so the registration can be shared.
pub type ClkNotifier = Arc<dyn Fn(&ClkEvent) -> NotifierResult + Send + Sync>;

/// Opaque token returned by notifier registration, used to unregister later.
pub type ClkNotifierId = u64;
