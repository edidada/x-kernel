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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
