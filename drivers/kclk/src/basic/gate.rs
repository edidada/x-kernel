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
        /// Write the gate bit atomically via the hiword-mask convention
        /// (Linux `CLK_GATE_HIWORD_MASK`): the upper 16 bits are a write mask,
        /// the lower 16 the value. This is a single `reg_write`, not a
        /// read-modify-write, so a gate bit sharing a register word with a
        /// divider/mux field cannot lose an update to the fast-path enable.
        /// Requires `bit_idx < 16`.
        const HIWORD_MASK = 1 << 1;
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

    /// Whether to use the hiword-mask atomic write (single `reg_write`) instead
    /// of a read-modify-write, so a shared-word gate bit does not race a
    /// divider/mux RMW under the same word.
    fn hiword(&self) -> bool {
        // The hiword-mask convention needs the gate bit in the low 16 bits so
        // the write mask (bit<<16) stays within the upper half.
        self.flags.contains(GateFlags::HIWORD_MASK) && self.bit_idx < 16
    }

    /// Write the gate bit. Atomic (single write) when `hiword`, else RMW.
    fn write_gate(&self, enabled: bool) {
        if self.hiword() {
            let value = (self.mask() << 16) | self.enable_value(enabled);
            self.reg.reg_write(self.offset, value);
        } else {
            self.reg
                .reg_modify(self.offset, self.mask(), self.enable_value(enabled));
        }
    }
}

impl ClkHwOps for Gate {
    fn enable(&self) -> ClkResult {
        self.write_gate(true);
        Ok(())
    }

    fn disable(&self) {
        self.write_gate(false);
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
