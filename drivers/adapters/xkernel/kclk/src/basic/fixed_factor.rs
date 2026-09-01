// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fixed-factor clock: scales the parent rate by a constant `mult`/`div`.

use alloc::sync::Arc;

use crate::{ClkError, ClkHwOps, ClkResult, Rate, RoundResult};

/// A clock that multiplies the parent rate by `mult` and divides by `div`.
///
/// The ratio is fixed in hardware (no programmable divider), so the node
/// neither changes rate nor gates; it only scales its parent. Typical use is a
/// fixed PLL post-divider or an integer relationship between two domains.
pub struct FixedFactor {
    mult: u32,
    div: u32,
}

impl FixedFactor {
    /// Describe a fixed-factor clock.
    ///
    /// Returns [`ClkError::InvalidRate`] if either factor is zero.
    pub const fn new(mult: u32, div: u32) -> ClkResult<Self> {
        if mult == 0 || div == 0 {
            return Err(ClkError::InvalidRate);
        }
        Ok(Self { mult, div })
    }

    /// Upcast into a clock-ops trait object for [`crate::ClkBuilder::new`].
    pub fn into_ops(self) -> Arc<dyn ClkHwOps> {
        Arc::new(self)
    }

    /// Scale `parent_rate` by the fixed ratio, guarding against overflow.
    fn scale(&self, parent_rate: Rate) -> ClkResult<Rate> {
        let scaled = parent_rate
            .checked_mul(self.mult as u64)
            .ok_or(ClkError::InvalidRate)?;
        scaled
            .checked_div(self.div as u64)
            .ok_or(ClkError::InvalidRate)
    }
}

impl ClkHwOps for FixedFactor {
    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        self.scale(parent_rate)
    }

    fn round_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<RoundResult> {
        // Current achievable rate is the parent scaled by the fixed ratio; to
        // reach `rate` we would need the parent to output rate*div/mult.
        let current = self.scale(parent_rate)?;
        let parent_needed = rate
            .checked_mul(self.div as u64)
            .and_then(|r| r.checked_div(self.mult as u64))
            .ok_or(ClkError::InvalidRate)?;
        Ok(RoundResult {
            rate: current,
            parent_target: Some(parent_needed),
        })
    }

    // No set_rate (immutable ratio), no gating, one parent (default).
}
