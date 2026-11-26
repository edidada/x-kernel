// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Register access for register-backed clock nodes.
//!
//! Register-backed basic types ([`crate::basic::Gate`], [`crate::basic::Mux`],
//! [`crate::basic::Divider`]) read and write control fields in a register
//! window. They reach the window only through the [`ClkRegAccess`] trait: this
//! crate owns no MMIO mechanism of its own. The host adapter supplies the
//! production accessor (a validated mapping in the driver layer), and tests
//! inject an array-backed mock so register field math can be exercised without
//! hardware.

use crate::{ClkError, ClkResult};

/// Read/write a clock's control register window.
///
/// Offsets are byte offsets from the window base and must be word-aligned and
/// within the window. Constructors reject a descriptor the accessor cannot
/// legally serve (see [`window_bytes`](Self::window_bytes)), so implementations
/// may treat an in-range offset as an established caller invariant.
pub trait ClkRegAccess: Send + Sync {
    /// Read the 32-bit word at `offset`.
    fn reg_read(&self, offset: usize) -> u32;
    /// Write the 32-bit word `value` to `offset`.
    fn reg_write(&self, offset: usize, value: u32);
    /// Read-modify-write: clear `mask` bits then set them to `set`.
    ///
    /// Default implementation reads, masks, and writes back. Providers whose
    /// hardware supports an atomic set/clear register pair may override this.
    fn reg_modify(&self, offset: usize, mask: u32, set: u32) {
        let value = (self.reg_read(offset) & !mask) | (set & mask);
        self.reg_write(offset, value);
    }

    /// Size in bytes of the register window this accessor can reach, when the
    /// implementation knows it.
    ///
    /// Node constructors reject a descriptor whose register word
    /// (`offset_bytes .. offset_bytes + 4`) leaves this window, so a malformed
    /// table fails with [`ClkError::InvalidValue`] during registration instead
    /// of touching memory the mapping does not cover. `None` means the window
    /// is not expressible here (test doubles, [`NullClkReg`]) and the offset is
    /// not bounds-checked.
    fn window_bytes(&self) -> Option<usize> {
        None
    }
}

/// Reject a register-word access this accessor cannot legally serve.
///
/// `offset_bytes` reaches the basic-type constructors from provider tables and
/// public constructors, so a value that is unaligned or outside the window is a
/// reachable configuration error: report it as [`ClkError::InvalidValue`]
/// rather than letting a correct-looking safe access fault.
pub(crate) fn check_word_access(reg: &dyn ClkRegAccess, offset_bytes: usize) -> ClkResult {
    if !offset_bytes.is_multiple_of(4) {
        return Err(ClkError::InvalidValue);
    }
    match reg.window_bytes() {
        Some(window) if offset_bytes.saturating_add(4) > window => Err(ClkError::InvalidValue),
        _ => Ok(()),
    }
}

/// A register accessor that touches no hardware: reads return 0, writes drop.
///
/// For clock trees whose nodes are purely computational (`FixedRate`,
/// `FixedFactor`) — `register_table` requires an accessor, but these types
/// never dereference it. Using this avoids inventing a fake MMIO address for
/// board-level fixed clocks described directly in firmware.
pub struct NullClkReg;

impl ClkRegAccess for NullClkReg {
    fn reg_read(&self, _offset: usize) -> u32 {
        0
    }

    fn reg_write(&self, _offset: usize, _value: u32) {}
}
