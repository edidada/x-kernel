// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Divider clock: divides the parent rate by a value read from a register.

use alloc::sync::Arc;

use bitflags::bitflags;

use crate::{ClkError, ClkHwOps, ClkRegAccess, ClkResult, Rate, RoundResult};

bitflags! {
    /// Policy bits for a [`Divider`]. Semantics match Linux's
    /// `CLK_DIVIDER_*` flags (`include/linux/clk-provider.h`).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct DividerFlags: u32 {
        /// `divider = raw` (register value 1 means ÷1, 2 means ÷2, ...).
        /// Matches Linux `CLK_DIVIDER_ONE_BASED`.
        ///
        /// When clear (the default, matching Linux's divider default), the
        /// encoding is zero-based: `divider = raw + 1`, so a freshly-reset
        /// all-zero field is a ÷1 pass-through.
        const ONE_BASED = 1 << 0;
        /// Divider values are powers of two (1, 2, 4, ...); the register value
        /// is the exponent. Matches Linux `CLK_DIVIDER_POWER_OF_TWO`. Implies
        /// the achievable rates are sparse.
        const POWER_OF_TWO = 1 << 1;
    }
}

/// A clock that derives its rate by dividing the parent rate.
///
/// The divider field occupies `width` bits at `shift` within the word at
/// `offset`. When a `table` is given, it lists the explicit divider values for
/// each register value (sparse or non-monotonic dividers); otherwise dividers
/// are linear, optionally power-of-two.
pub struct Divider {
    reg: Arc<dyn ClkRegAccess>,
    offset: usize,
    shift: u8,
    width: u8,
    /// Linux-style `(register value, divider)` pairs; `val` entries are
    /// matched by linear scan (matching Linux `_get_table_div`), so sparse
    /// encodings work without padding holes.
    table: Option<&'static [DivTableEntry]>,
    flags: DividerFlags,
}

/// One `(register value, divider)` row of a divider table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DivTableEntry {
    /// The raw register field value.
    pub val: u32,
    /// The divider it represents.
    pub div: u32,
}

impl DivTableEntry {
    /// Construct a `(val, div)` row.
    pub const fn new(val: u32, div: u32) -> Self {
        Self { val, div }
    }
}

impl Divider {
    /// Create a divider clock.
    ///
    /// `width` must be in `1..32`; a width of 32 would shift a `u32` out of
    /// range (undefined behavior) and is rejected with a debug assertion.
    pub fn new(
        reg: Arc<dyn ClkRegAccess>,
        offset: usize,
        shift: u8,
        width: u8,
        table: Option<&'static [DivTableEntry]>,
        flags: DividerFlags,
    ) -> Self {
        // Soundness: `1u32 << width` is UB at width >= 32. This is a driver
        // configuration error, so fail hard (not debug_assert, which is
        // compiled out of release builds where the UB would be silent).
        assert!(width > 0 && width < 32, "divider width must be in 1..32");
        Self {
            reg,
            offset,
            shift,
            width,
            table,
            flags,
        }
    }

    /// Construct and upcast in one step for [`crate::ClkBuilder::new`].
    #[allow(clippy::too_many_arguments)]
    pub fn into_ops(
        reg: Arc<dyn ClkRegAccess>,
        offset: usize,
        shift: u8,
        width: u8,
        table: Option<&'static [DivTableEntry]>,
        flags: DividerFlags,
    ) -> Arc<dyn ClkHwOps> {
        Arc::new(Self::new(reg, offset, shift, width, table, flags))
    }

    /// Field mask for `width` bits.
    fn field_mask(&self) -> u32 {
        (1u32 << self.width) - 1
    }

    /// Maximum achievable divider value.
    fn max_div(&self) -> u32 {
        if let Some(table) = self.table {
            return table.iter().map(|e| e.div).max().unwrap_or(1).max(1);
        }
        if self.flags.contains(DividerFlags::POWER_OF_TWO) {
            1u32 << self.width
        } else if self.flags.contains(DividerFlags::ONE_BASED) {
            // Raw values [1, field_mask] map 1:1 to dividers.
            self.field_mask()
        } else {
            // Zero-based: raw values [0, field_mask] map to [1, 2^width].
            1u32 << self.width
        }
    }

    /// Read the raw register field value.
    fn raw(&self) -> u32 {
        (self.reg.reg_read(self.offset) >> self.shift) & self.field_mask()
    }

    /// Convert a raw register value to its divider.
    fn raw_to_div(&self, raw: u32) -> ClkResult<u32> {
        if let Some(table) = self.table {
            // A register value outside the table means uninitialized or
            // corrupted hardware (or a misdescribed table) — surface it
            // instead of silently decoding it as a legal ÷1 (Linux's
            // _get_table_div returns -EINVAL here).
            let entry = table
                .iter()
                .find(|e| e.val == raw)
                .ok_or(ClkError::BadState)?;
            return Ok(entry.div.max(1));
        }
        if self.flags.contains(DividerFlags::POWER_OF_TWO) {
            return Ok(1u32 << raw.min(self.width as u32));
        }
        if self.flags.contains(DividerFlags::ONE_BASED) {
            // raw 0 is not a valid one-based divider; clamp defensively.
            Ok(raw.max(1))
        } else {
            Ok(raw + 1)
        }
    }

    /// Convert a desired divider to the raw register value (closest).
    fn div_to_raw(&self, div: u32) -> ClkResult<u32> {
        if let Some(table) = self.table {
            // Pick the table entry whose divider is closest to `div`
            // (matching Linux `_get_table_val`).
            let mut best = table.first().map_or(0, |e| e.val);
            let mut best_diff = u64::MAX;
            for entry in table.iter() {
                let d = diff(div as u64, entry.div as u64);
                if d < best_diff {
                    best_diff = d;
                    best = entry.val;
                }
            }
            return Ok(best);
        }
        if self.flags.contains(DividerFlags::POWER_OF_TWO) {
            // floor(log2(div)), matching Linux's `fls(divider) - 1`: a
            // divider of 5 encodes as 2^2 = 4, never overshooting to 2^3.
            let exp = 31 - div.max(1).leading_zeros();
            return Ok(exp.min(self.width as u32));
        }
        if self.flags.contains(DividerFlags::ONE_BASED) {
            return Ok(div.min(self.field_mask()));
        }
        Ok(div.saturating_sub(1))
    }

    /// Find the divider giving a rate closest to `target` at `parent_rate`.
    fn closest_div(&self, parent_rate: Rate, target: Rate) -> u32 {
        if let Some(table) = self.table {
            let mut best = 1u32;
            let mut best_diff = u64::MAX;
            for d in table.iter().map(|e| e.div) {
                let r = rate_for(parent_rate, d);
                let df = diff(r, target);
                if df < best_diff {
                    best_diff = df;
                    best = d;
                }
            }
            return best.max(1);
        }
        if self.flags.contains(DividerFlags::POWER_OF_TWO) {
            let mut best = 1u32;
            let mut best_diff = u64::MAX;
            let mut d = 1u32;
            while d <= self.max_div() {
                let df = diff(rate_for(parent_rate, d), target);
                if df < best_diff {
                    best_diff = df;
                    best = d;
                }
                d <<= 1;
            }
            return best;
        }
        // Linear dividers: the divider minimizing |parent/div - target| is the
        // rounded quotient, clamped to the achievable range. Check its floor and
        // ceiling to absorb integer rounding at the boundary.
        let approx: u32 = parent_rate
            .checked_div(target)
            .map(|r| r.min(u32::MAX as u64) as u32)
            .unwrap_or(1);
        let lo = approx.saturating_sub(1).max(1);
        let hi = (approx + 1).min(self.max_div());
        let mut best = 1u32;
        let mut best_diff = u64::MAX;
        for d in lo..=hi {
            let r = rate_for(parent_rate, d);
            let df = diff(r, target);
            if df < best_diff {
                best_diff = df;
                best = d;
            }
        }
        best
    }
}

impl ClkHwOps for Divider {
    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        Ok(rate_for(parent_rate, self.raw_to_div(self.raw())?))
    }

    fn round_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<RoundResult> {
        let div = self.closest_div(parent_rate, rate);
        Ok(RoundResult {
            rate: rate_for(parent_rate, div),
            parent_target: None,
        })
    }

    fn set_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<Rate> {
        let div = self.closest_div(parent_rate, rate);
        let raw = self.div_to_raw(div)?;
        let field = (raw & self.field_mask()) << self.shift;
        self.reg
            .reg_modify(self.offset, self.field_mask() << self.shift, field);
        Ok(rate_for(parent_rate, self.raw_to_div(raw)?))
    }
}

/// `parent_rate / div`, treating a zero divider as pass-through.
fn rate_for(parent_rate: Rate, div: u32) -> Rate {
    parent_rate.checked_div(div as u64).unwrap_or(parent_rate)
}

/// Absolute difference of two rates.
fn diff(a: u64, b: u64) -> u64 {
    a.saturating_sub(b).max(b.saturating_sub(a))
}
