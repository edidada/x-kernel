// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock policy and topology flags.
//!
//! Several of these mirror the Linux `CLK_*` flags but are not yet honored by
//! the framework core (see each variant). They are kept in the type so a driver
//! can express its intent today and benefit once the core grows the matching
//! behavior; setting an unimplemented flag is a documented no-op, never a
//! silent functional guarantee.

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
        /// *Reserved — not yet honored.* Reserved for a future unused-clock
        /// gate-down pass; today the framework never auto-disables a clock, so
        /// the flag is a no-op. Do not rely on it for correctness.
        const IGNORE_UNUSED = 1 << 2;
        /// The clock is critical and must stay enabled at all times.
        ///
        /// *Reserved — not yet honored.* Intended for a future runtime PM /
        /// `clk_disable_unused` pass. Today the framework only disables a clock
        /// when its `enable_count` reaches zero, so setting this is a no-op; do
        /// not rely on it.
        const IS_CRITICAL = 1 << 3;
        /// Re-read the hardware rate on every query instead of trusting the cache.
        ///
        /// *Reserved — not yet honored.* `get_rate` always reads the cached
        /// value. Intended for clocks whose rate can change behind the
        /// framework's back; do not rely on it yet.
        const GET_RATE_NOCACHE = 1 << 6;
    }
}
