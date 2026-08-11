// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fixed-rate clock: constant frequency, no parent, no gating.

use alloc::sync::Arc;

use crate::{ClkHwOps, ClkResult, Rate, RoundResult};

/// A clock whose rate never changes and that has no parent.
///
/// Common examples are board-level crystals (`osc24m`, `osc32k`) and fixed
/// reference oscillators described directly in firmware.
pub struct FixedRate {
    rate: Rate,
}

impl FixedRate {
    /// Describe a fixed-rate clock of `rate` Hz.
    pub const fn new(rate: Rate) -> Self {
        Self { rate }
    }

    /// Upcast into a clock-ops trait object for [`crate::ClkBuilder::new`].
    pub fn into_ops(self) -> Arc<dyn ClkHwOps> {
        Arc::new(self)
    }
}

impl ClkHwOps for FixedRate {
    fn recalc_rate(&self, _parent_rate: Rate) -> ClkResult<Rate> {
        Ok(self.rate)
    }

    fn round_rate(&self, _rate: Rate, _parent_rate: Rate) -> ClkResult<RoundResult> {
        // Immutable: report the fixed rate; no parent change requested.
        Ok(RoundResult {
            rate: self.rate,
            parent_target: None,
        })
    }

    // set_rate intentionally left to the default (NotSupported): a fixed clock
    // cannot be reprogrammed. No gating or parent ops apply either.
}
