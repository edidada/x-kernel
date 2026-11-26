// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mux clock: selects one of several parents via a register field.

use alloc::sync::Arc;

use bitflags::bitflags;

use crate::{ClkError, ClkHwOps, ClkRateRequest, ClkRegAccess, ClkResult, Rate, RoundResult};

bitflags! {
    /// Policy bits for a [`Mux`]. Semantics match Linux's `CLK_MUX_*` flags
    /// (`include/linux/clk-provider.h`).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct MuxFlags: u32 {
        /// Write the parent-select field atomically via the hiword-mask
        /// convention (Linux `CLK_MUX_HIWORD_MASK`): the upper 16 bits of the
        /// written word are the write-enable mask, the lower 16 the value.
        /// A single `reg_write`, not a read-modify-write, so a parent switch
        /// cannot lose a race against a gate's fast-path enable on a shared
        /// register word (Rockchip CRU style). Requires `shift + width <= 16`.
        const HIWORD_MASK = 1 << 0;
    }
}

/// A clock that picks one of `N` parents using a bit field in a register.
///
/// The field occupies `width` bits at `shift` within the word at `offset_bytes`
/// (a byte offset into the controller's register window).
/// When a `table` is given it maps *parent index → register value*
/// (`table[index] = raw`, matching Linux's `clk_mux` table direction), so
/// sparse or reordered hardware encodings list only the parents they use;
/// otherwise the register value is the parent index directly.
pub struct Mux {
    reg: Arc<dyn ClkRegAccess>,
    offset_bytes: usize,
    shift: u8,
    mask: u32,
    table: Option<&'static [u8]>,
    flags: MuxFlags,
    /// Parents this mux can encode, validated at construction because the
    /// framework's parent index is a `u8`.
    num_parents: u8,
}

impl Mux {
    /// Create a mux.
    ///
    /// `offset_bytes` is a byte offset into the controller's register window;
    /// it must be word-aligned and inside the window the accessor reports.
    /// `mask` is the *unshifted* field mask (e.g. `0x7` for a 3-bit field); it
    /// is applied after `>> shift`.
    ///
    /// The number of parents follows from the table length or the field mask.
    /// More than 255 parents cannot be addressed by the framework's `u8` parent
    /// index, so that configuration is rejected with
    /// [`ClkError::InvalidValue`] (a truncated count would silently make the
    /// high parents unreachable).
    pub fn new(
        reg: Arc<dyn ClkRegAccess>,
        offset_bytes: usize,
        shift: u8,
        mask: u32,
        table: Option<&'static [u8]>,
        flags: MuxFlags,
    ) -> ClkResult<Self> {
        crate::reg::check_word_access(&*reg, offset_bytes)?;
        // `mask + 1` overflows for a full-width field, so widen before adding.
        let count = match table {
            Some(table) => table.len() as u64,
            None => u64::from(mask) + 1,
        };
        let num_parents = u8::try_from(count).map_err(|_| ClkError::InvalidValue)?;
        Ok(Self {
            reg,
            offset_bytes,
            shift,
            mask,
            table,
            flags,
            num_parents,
        })
    }

    /// Construct and upcast in one step for [`crate::ClkBuilder::new`].
    pub fn into_ops(
        reg: Arc<dyn ClkRegAccess>,
        offset_bytes: usize,
        shift: u8,
        mask: u32,
        table: Option<&'static [u8]>,
        flags: MuxFlags,
    ) -> ClkResult<Arc<dyn ClkHwOps>> {
        Ok(Arc::new(Self::new(
            reg,
            offset_bytes,
            shift,
            mask,
            table,
            flags,
        )?))
    }

    /// Read the raw register field value.
    fn raw(&self) -> u32 {
        (self.reg.reg_read(self.offset_bytes) >> self.shift) & self.mask
    }

    /// Translate a register value to a parent index.
    ///
    /// With a table (`table[index] = raw`), scan for the index whose raw
    /// value matches — matching Linux `clk_mux_get_parent`.
    fn raw_to_index(&self, raw: u32) -> ClkResult<u8> {
        match self.table {
            Some(table) => u8::try_from(
                table
                    .iter()
                    .position(|&r| u32::from(r) == raw)
                    .ok_or(ClkError::BadState)?,
            )
            .map_err(|_| ClkError::BadState),
            None => u8::try_from(raw).map_err(|_| ClkError::BadState),
        }
    }

    /// Translate a parent index to the register value to write.
    fn index_to_raw(&self, index: u8) -> ClkResult<u32> {
        match self.table {
            Some(table) => Ok(u32::from(
                *table.get(index as usize).ok_or(ClkError::InvalidParent)?,
            )),
            None => Ok(u32::from(index)),
        }
    }

    /// Whether to use the hiword-mask atomic write (single `reg_write`) instead
    /// of a read-modify-write. The hiword convention needs the field within the
    /// low 16 bits so the write mask (`field << 16`) stays in the upper half.
    ///
    /// A zero mask selects a single parent with no bit in the word: there is no
    /// field to place in either half, so the atomic form never applies.
    fn hiword(&self) -> bool {
        self.mask != 0
            && self.flags.contains(MuxFlags::HIWORD_MASK)
            && self.shift as u32 + (31 - self.mask.leading_zeros()) < 16
    }
}

impl ClkHwOps for Mux {
    fn num_parents(&self) -> u8 {
        self.num_parents
    }

    fn get_parent(&self) -> ClkResult<u8> {
        self.raw_to_index(self.raw())
    }

    fn set_parent(&self, index: u8) -> ClkResult {
        let raw = self.index_to_raw(index)?;
        let field = (raw & self.mask) << self.shift;
        if self.hiword() {
            // Atomic hiword write: the upper half carries the write-enable
            // mask, so other fields in this word are untouched in one store.
            let mask = self.mask << (self.shift + 16);
            self.reg.reg_write(self.offset_bytes, mask | field);
        } else {
            self.reg
                .reg_modify(self.offset_bytes, self.mask << self.shift, field);
        }
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
        // Honor the request's min/max bounds when picking a source: prefer a
        // parent whose output already lands inside the allowed range, and
        // only fall back to the closest overall when none does.
        let Some(&best) = parents.iter().min_by_key(|(_, prate)| {
            let in_range = req.min_rate <= *prate && *prate <= req.max_rate;
            // Sort by (out-of-range, |diff|): in-range parents win first.
            (!in_range, diff(requested, *prate))
        }) else {
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
