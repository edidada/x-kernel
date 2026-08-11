// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Register access for register-backed clock nodes.
//!
//! Register-backed basic types ([`crate::basic::Gate`], [`crate::basic::Mux`],
//! [`crate::basic::Divider`]) read and write control fields in a register
//! window. To stay free of any host MMIO mechanism, they talk to the window
//! through the [`ClkRegAccess`] trait. Production uses [`MmioClkReg`] (volatile
//! loads/stores over a mapped virtual address); tests inject an array-backed
//! mock so register field math can be exercised without real hardware.

use core::ptr;

/// Read/write a clock's control register window.
///
/// Offsets are byte offsets from the window base and must be word-aligned and
/// within the window; the implementation is responsible for the access width
/// and ordering.
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
}

/// MMIO register window backed by a mapped virtual address.
///
/// The window is treated as process-global and stable for the clock tree's
/// lifetime, so sharing it across CPUs is sound. The pointer is non-zero and
/// word-aligned by construction (the caller of [`new`](Self::new) guarantees
/// both).
pub struct MmioClkReg {
    base: *mut u8,
}

// SAFETY: MMIO register windows are mapped once at boot, are process-global,
// and never move. Volatile loads/stores are safe from any CPU because the
// device register semantics tolerate concurrent reads and the framework
// serializes writes per clock node.
unsafe impl Send for MmioClkReg {}
// SAFETY: same rationale as `Send`; the window has no per-thread affinity.
unsafe impl Sync for MmioClkReg {}

impl MmioClkReg {
    /// Wrap a mapped MMIO virtual base.
    ///
    /// # Safety contract for the caller
    ///
    /// `base` must point to a valid, word-aligned, mapped register window that
    /// remains live for the lifetime of every clock node that shares it.
    pub fn new(base: usize) -> Self {
        Self {
            base: base as *mut u8,
        }
    }
}

impl ClkRegAccess for MmioClkReg {
    fn reg_read(&self, offset: usize) -> u32 {
        // SAFETY: the caller of `new` guaranteed `base` is a valid mapped MMIO
        // window and `offset` is within it and word-aligned; volatile prevents
        // the compiler from eliding or reordering the device read.
        unsafe { ptr::read_volatile(self.base.add(offset) as *const u32) }
    }

    fn reg_write(&self, offset: usize, value: u32) {
        // SAFETY: as above; the volatile store orders the device write so it is
        // not combined with neighboring stores.
        unsafe { ptr::write_volatile(self.base.add(offset) as *mut u32, value) }
    }
}
