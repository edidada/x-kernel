// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `ClkRegAccess` over a validated MMIO mapping.
//!
//! The clock framework owns no MMIO mechanism: a controller driver hands it a
//! [`ClkRegAccess`]. This module is that implementation for real hardware. It
//! stores an [`IoMap`] instead of a raw address, so every access goes through
//! the mapping's bounds and alignment checks, and the mapping is released by the
//! `Arc` the clock tree holds when the tree is torn down.

use driver_os_api::{IoMap, MmioRegion};
use kclk::ClkRegAccess;

/// [`ClkRegAccess`] backed by a mapped register window.
pub struct IoMapClkReg {
    map: IoMap,
}

impl IoMapClkReg {
    /// Wrap an established mapping.
    ///
    /// The mapping already carries the region size and a teardown hook, so the
    /// clock tree inherits both the window bound reported to
    /// [`ClkRegAccess::window_bytes`] and the unmap on drop.
    pub const fn new(map: IoMap) -> Self {
        Self { map }
    }

    /// The mapped region, for probe-time diagnostics.
    pub const fn region(&self) -> MmioRegion {
        self.map.region()
    }
}

impl ClkRegAccess for IoMapClkReg {
    fn reg_read(&self, offset: usize) -> u32 {
        self.map.read32(offset)
    }

    fn reg_write(&self, offset: usize, value: u32) {
        self.map.write32(offset, value);
    }

    fn window_bytes(&self) -> Option<usize> {
        Some(self.map.region().size)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/clk_reg.rs");
