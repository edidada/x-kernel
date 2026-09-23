// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error and result types for the clock subsystem.

/// Errors returned by clock operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClkError {
    /// No clock provider has been installed yet.
    NoProvider,
    /// The named or indexed clock does not exist.
    NotFound,
    /// The hardware operation is not implemented by this clock.
    NotSupported,
    /// The requested rate is outside the clock's achievable range.
    InvalidRate,
    /// The requested parent index is out of range or unmapped.
    InvalidParent,
    /// The clock is busy and the operation cannot complete right now.
    Busy,
    /// The hardware driver reported a generic I/O failure.
    Io,
    /// The clock is in an inconsistent internal state.
    BadState,
    /// A rate-change subscriber vetoed the change.
    Aborted,
}

/// Result alias for clock operations.
pub type ClkResult<T = ()> = Result<T, ClkError>;
