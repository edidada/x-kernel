// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mux clock: selects one of several parents via a register field.

use alloc::sync::Arc;

use crate::{ClkError, ClkHwOps, ClkRateRequest, ClkRegAccess, ClkResult, Rate, RoundResult};

/// A clock that picks one of `N` parents using a bit field in a register.
///
/// The field occupies `width` bits at `shift` within the word at `offset`.
/// When a `table` is given, register values are translated to parent indices
/// through it (useful when the hardware encoding is sparse or reordered);
/// otherwise the register value is the parent index directly.
pub struct Mux {
    reg: Arc<dyn ClkRegAccess>,
    offset: usize,
    shift: u8,
    mask: u32,
    table: Option<&'static [u8]>,
}

impl Mux {
    /// Create a mux.
    ///
    /// `mask` is the *unshifted* field mask (e.g. `0x7` for a 3-bit field); it
    /// is applied after `>> shift`.
    pub fn new(
        reg: Arc<dyn ClkRegAccess>,
        offset: usize,
        shift: u8,
        mask: u32,
        table: Option<&'static [u8]>,
    ) -> Self {
        Self {
            reg,
            offset,
            shift,
            mask,
            table,
        }
    }

    /// Construct and upcast in one step for [`crate::ClkBuilder::new`].
    pub fn into_ops(
        reg: Arc<dyn ClkRegAccess>,
        offset: usize,
        shift: u8,
        mask: u32,
        table: Option<&'static [u8]>,
    ) -> Arc<dyn ClkHwOps> {
        Arc::new(Self::new(reg, offset, shift, mask, table))
    }

    /// Read the raw register field value.
    fn raw(&self) -> u32 {
        (self.reg.reg_read(self.offset) >> self.shift) & self.mask
    }

    /// Translate a register value to a parent index.
    fn raw_to_index(&self, raw: u32) -> ClkResult<u8> {
        match self.table {
            // The table maps consecutive register values to parent indices.
            Some(table) => table.get(raw as usize).copied().ok_or(ClkError::BadState),
            None => u8::try_from(raw).map_err(|_| ClkError::BadState),
        }
    }

    /// Translate a parent index to the register value to write.
    fn index_to_raw(&self, index: u8) -> ClkResult<u32> {
        match self.table {
            Some(table) => table
                .iter()
                .position(|&p| p == index)
                .map(|p| p as u32)
                .ok_or(ClkError::InvalidParent),
            None => Ok(index as u32),
        }
    }
}

impl ClkHwOps for Mux {
    fn num_parents(&self) -> u8 {
        let count = match self.table {
            Some(table) => table.len(),
            None => (self.mask + 1) as usize,
        };
        // The framework's parent index is a `u8`; a field that can encode 256
        // values cannot be represented. Cap defensively rather than wrap to 0.
        count.min(u8::MAX as usize) as u8
    }

    fn get_parent(&self) -> ClkResult<u8> {
        self.raw_to_index(self.raw())
    }

    fn set_parent(&self, index: u8) -> ClkResult {
        let raw = self.index_to_raw(index)?;
        let field = (raw & self.mask) << self.shift;
        self.reg
            .reg_modify(self.offset, self.mask << self.shift, field);
        Ok(())
    }

    fn recalc_rate(&self, parent_rate: Rate) -> ClkResult<Rate> {
        // A mux is rate-transparent.
        Ok(parent_rate)
    }

    fn round_rate(&self, rate: Rate, parent_rate: Rate) -> ClkResult<RoundResult> {
        // Transparent: achieve the parent rate, ask the parent for `rate`.
        Ok(RoundResult {
            rate: parent_rate,
            parent_target: Some(rate),
        })
    }

    fn determine_rate(&self, req: &mut ClkRateRequest, parents: &[(u8, Rate)]) -> ClkResult<()> {
        let requested = req.rate;
        let Some(&best) = parents
            .iter()
            .min_by_key(|(_, prate)| diff(requested, *prate))
        else {
            return Ok(());
        };
        req.best_parent_index = Some(best.0);
        req.best_parent_rate = best.1;
        req.rate = best.1; // transparent: achievable rate is the parent's rate
        // A single-parent mux cannot switch sources; to satisfy the request the
        // framework must change that parent's rate (upward propagation). With
        // multiple parents, switching to the closest source already satisfies
        // the request, so no parent rate change is requested.
        req.parent_target = (parents.len() == 1).then_some(requested);
        Ok(())
    }
}

/// Absolute difference between two rates.
fn diff(a: Rate, b: Rate) -> Rate {
    a.saturating_sub(b).max(b.saturating_sub(a))
}
