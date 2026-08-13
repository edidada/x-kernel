// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock policy and topology flags.
//!
//! These mirror the Linux `CLK_*` flags. Each variant documents whether it is
//! honored by the framework core.

use bitflags::bitflags;

bitflags! {
    /// Flags governing a clock node's behavior.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ClkFlags: u32 {
        /// Propagate rate changes up to the parent clock.
        ///
        /// **Implemented:** honored by `ClkCore::round_rate` / `set_rate`.
        const SET_RATE_PARENT = 1 << 0;
        /// Gate the clock while its rate or parent is being changed, to avoid
        /// glitches on running hardware.
        ///
        /// **Implemented:** `set_rate` / `set_parent` save the gate state,
        /// disable the clock across the change, and restore it afterwards.
        const SET_RATE_GATE = 1 << 1;
        /// Never gate this clock, even when it appears unused.
        ///
        /// **Implemented:** honored by [`crate::clk_disable_unused`], which
        /// skips clocks carrying this flag.
        const IGNORE_UNUSED = 1 << 2;
        /// The clock is critical and must stay enabled at all times.
        ///
        /// **Implemented:** honored by [`crate::clk_disable_unused`], which
        /// skips clocks carrying this flag.
        const IS_CRITICAL = 1 << 3;
        /// Re-read the hardware rate on every query instead of trusting the cache.
        ///
        /// **Implemented:** `get_rate` re-reads the hardware rate (and refreshes
        /// the cache) instead of returning the cached value.
        const GET_RATE_NOCACHE = 1 << 6;
    }
}
