// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reset lines: one hardware line the driver can assert, deassert and
//! sample, addressed by an absolute position in a provider's register
//! window.
//!
//! Unlike a clock, a reset line carries no rate topology and no enable
//! refcount: Linux's `reset_control_deassert/assert` translate one-to-one
//! onto [`ResetLine`] methods, and Rockchip's `softrst` convention
//! (hiword write mask, 16 lines per word) is the only hardware semantic
//! baked in here.

use alloc::sync::Arc;

/// Read/write a reset controller's register window.
///
/// Deliberately a separate trait from `kclk::ClkRegAccess`: a reset
/// controller is not a clock, and folding them would couple two
/// subsystems' vocabularies for the sake of ~20 duplicated lines. The
/// RK3588 CRU driver backs both traits with the same mapped window.
pub trait ResetRegAccess: Send + Sync {
    /// Read the 32-bit word at `offset` (byte offset from the window base).
    fn reg_read(&self, offset: usize) -> u32;
    /// Write the 32-bit word `value` at `offset`.
    fn reg_write(&self, offset: usize, value: u32);
}

/// One reset line, pre-resolved to its absolute register position.
///
/// `word_offset` is the byte offset of the line's control word relative to
/// the [`ResetRegAccess`] window base (for Rockchip: `bank * 4`), `bit` the
/// line's position in that word (`0..16` — the hiword half carries the
/// write-enable mask).
#[derive(Clone)]
pub struct ResetLine {
    reg: Arc<dyn ResetRegAccess>,
    word_offset: usize,
    bit: u8,
}

impl ResetLine {
    /// Bind a line to its control word and bit.
    ///
    /// `bit` must stay below 16: the hiword write convention needs the
    /// value half in the low word (`bit` position) and the write-enable
    /// mask in the high word (`bit + 16`).
    pub fn new(reg: Arc<dyn ResetRegAccess>, word_offset: usize, bit: u8) -> Option<Self> {
        (bit < 16).then(|| Self {
            reg,
            word_offset,
            bit,
        })
    }

    fn write_line(&self, set: bool) {
        let mask = 1u32 << self.bit;
        // Hiword convention: a single store carries the write-enable mask in
        // the high half and the target value in the low half — no
        // read-modify-write, so a line update never loses against a
        // concurrent update of a neighbouring line in the same word.
        let value = if set { mask } else { 0 };
        self.reg.reg_write(self.word_offset, (mask << 16) | value);
    }

    /// Assert the line (hold the IP in reset). Rockchip convention: the
    /// bit set means asserted.
    pub fn assert(&self) {
        self.write_line(true);
    }

    /// Deassert the line (release the IP out of reset).
    pub fn deassert(&self) {
        self.write_line(false);
    }

    /// Sample the line: `true` when asserted (Rockchip: the bit reads set).
    pub fn status(&self) -> bool {
        self.reg.reg_read(self.word_offset) & (1 << self.bit) != 0
    }
}

/// A borrowed line plus its identity — what a consumer holds after
/// acquiring a reset. Cheap to clone; the ops are explicit calls (no RAII
/// refcount, matching Linux `reset_control`), with one convenience guard
/// for the probe-time deassert pattern.
#[derive(Clone)]
pub struct ResetHandle {
    line: Arc<ResetLine>,
}

impl ResetHandle {
    pub(crate) fn new(line: Arc<ResetLine>) -> Self {
        Self { line }
    }

    /// Assert the line (hold the IP in reset).
    pub fn assert(&self) {
        self.line.assert();
    }

    /// Deassert the line (release the IP out of reset).
    pub fn deassert(&self) {
        self.line.deassert();
    }

    /// Sample the line: `true` when asserted.
    pub fn status(&self) -> bool {
        self.line.status()
    }

    /// Deassert and return a guard that re-asserts on drop.
    ///
    /// The probe-time shape: release the IP out of reset for the lifetime
    /// of the binding. The guard is informational ownership — the line
    /// itself is boot-static, so a forgotten guard never corrupts state.
    pub fn deasserted(&self) -> DeassertedGuard {
        self.line.deassert();
        DeassertedGuard {
            line: self.line.clone(),
        }
    }
}

/// Guard that re-asserts its line on drop (the reverse of
/// `devm_reset_control_deassert`'s cleanup).
pub struct DeassertedGuard {
    line: Arc<ResetLine>,
}

impl Drop for DeassertedGuard {
    fn drop(&mut self) {
        self.line.assert();
    }
}
