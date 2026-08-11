// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Gate clock: toggles a single enable bit in a register.

use alloc::sync::Arc;

use bitflags::bitflags;

use crate::{ClkHwOps, ClkRegAccess, ClkResult, Rate, RoundResult};

bitflags! {
    /// Policy bits for a [`Gate`].
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct GateFlags: u32 {
        /// Writing a 1 to the bit enables the clock; otherwise writing a 0
        /// enables it (hardware inversion, e.g. an "is_disabled" bit).
        const SET_TO_ENABLE = 1 << 0;
    }
}

/// A clock whose only effect is to enable/disable a single register bit.
///
/// Rate is passed through unchanged from the selected parent. The bit lives in
/// a 32-bit word at `offset` within the register window; `bit_idx` selects the
/// bit position (0–31).
pub struct Gate {
    reg: Arc<dyn ClkRegAccess>,
    offset: usize,
    bit_idx: u8,
    flags: GateFlags,
}

impl Gate {
    /// Create a gate clock.
    ///
    /// `bit_idx` must be < 32; the provider of [`ClkRegAccess`] is responsible
    /// for the window being mapped and word-aligned.
    pub fn new(reg: Arc<dyn ClkRegAccess>, offset: usize, bit_idx: u8, flags: GateFlags) -> Self {
        Self {
            reg,
            offset,
            bit_idx,
            flags,
        }
    }

    /// Construct and upcast in one step for [`crate::ClkBuilder::new`].
    pub fn into_ops(
        reg: Arc<dyn ClkRegAccess>,
        offset: usize,
        bit_idx: u8,
        flags: GateFlags,
    ) -> Arc<dyn ClkHwOps> {
        Arc::new(Self::new(reg, offset, bit_idx, flags))
    }

    fn mask(&self) -> u32 {
        1u32 << self.bit_idx
    }

    /// Register value to OR in for the desired logical `enabled` state.
    fn enable_value(&self, enabled: bool) -> u32 {
        let bit_should_be_set = enabled == self.flags.contains(GateFlags::SET_TO_ENABLE);
        if bit_should_be_set { self.mask() } else { 0 }
    }
}

impl ClkHwOps for Gate {
    fn enable(&self) -> ClkResult {
        self.reg
            .reg_modify(self.offset, self.mask(), self.enable_value(true));
        Ok(())
    }

    fn disable(&self) {
        self.reg
            .reg_modify(self.offset, self.mask(), self.enable_value(false));
    }

    fn is_enabled(&self) -> ClkResult<bool> {
        let bit = (self.reg.reg_read(self.offset) >> self.bit_idx) & 1;
        let clock_on = if self.flags.contains(GateFlags::SET_TO_ENABLE) {
            bit != 0
        } else {
            bit == 0
        };
        Ok(clock_on)
    }

    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        // A gate is rate-transparent.
        Ok(parent_rate)
    }

    fn round_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<RoundResult> {
        // Transparent: achieve the parent rate, ask the parent for `rate`.
        Ok(RoundResult {
            rate: parent_rate,
            parent_target: Some(rate),
        })
    }
}
