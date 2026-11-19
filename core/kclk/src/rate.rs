// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock rate primitives.
//!
//! Rates are always expressed in Hz as unsigned 64-bit integers, matching the
//! Linux common clock framework convention.

/// Clock frequency, in Hz.
pub type Rate = u64;

/// Outcome of rounding a rate request, with an optional parent request.
///
/// [`ClkHwOps::round_rate`](crate::ClkHwOps::round_rate) returns this so a node
/// can both report the closest rate it can reach *and*, when it cannot satisfy
/// the request on its own, ask the framework to change its parent's rate. The
/// framework honors `parent_target` only when the node carries
/// [`ClkFlags::SET_RATE_PARENT`](crate::ClkFlags::SET_RATE_PARENT).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoundResult {
    /// Closest rate this node can achieve for the request.
    pub rate: Rate,
    /// Rate the parent should output so this node can reach `rate`, or `None`
    /// when this node satisfies the request on its own.
    pub parent_target: Option<Rate>,
}

impl RoundResult {
    /// A node that cannot change rate at all: report the parent rate, do not
    /// request a parent change.
    pub const fn passthrough(parent_rate: Rate) -> Self {
        Self {
            rate: parent_rate,
            parent_target: None,
        }
    }
}

/// A rate-determination request passed to
/// [`ClkHwOps::determine_rate`](crate::ClkHwOps::determine_rate).
///
/// Mirrors Linux's `struct clk_rate_request`: the provider fills in the closest
/// achievable `rate`, the parent it would select for that rate
/// (`best_parent_index` / `best_parent_rate`), and — when it is
/// rate-transparent — the rate the parent must output (`parent_target`) so the
/// framework can propagate the change upward.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClkRateRequest {
    /// Requested rate, already clamped by the core into
    /// [`min_rate`](Self::min_rate)..=[`max_rate`](Self::max_rate).
    pub rate: Rate,
    /// Lower bound on the achievable rate.
    pub min_rate: Rate,
    /// Upper bound on the achievable rate.
    pub max_rate: Rate,
    /// The parent the provider selected for this request. The core seeds this
    /// with the currently selected parent; a provider may override it to switch
    /// parents (a mux picking the closest source).
    pub best_parent_index: Option<u8>,
    /// The rate of [`best_parent_index`](Self::best_parent_index).
    pub best_parent_rate: Rate,
    /// The rate the selected parent should be changed to so this node can
    /// reach `rate`, or `None` when this node satisfies the request on its own
    /// (or by switching parents). Honored only when the node carries
    /// [`ClkFlags::SET_RATE_PARENT`](crate::ClkFlags::SET_RATE_PARENT).
    pub parent_target: Option<Rate>,
}
