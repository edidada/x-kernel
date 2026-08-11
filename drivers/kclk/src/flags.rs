// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock policy and topology flags.

use bitflags::bitflags;

bitflags! {
    /// Flags governing a clock node's behavior.
    ///
    /// Mirrors the `CLK_*` flags from the Linux common clock framework.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ClkFlags: u32 {
        /// Propagate rate changes up to the parent clock.
        const SET_RATE_PARENT = 1 << 0;
        /// Gate the clock while its rate is being changed.
        const SET_RATE_GATE = 1 << 1;
        /// Never gate this clock, even when it appears unused.
        const IGNORE_UNUSED = 1 << 2;
        /// The clock is critical and must stay enabled at all times.
        const IS_CRITICAL = 1 << 3;
        /// Re-read the hardware rate on every query instead of trusting the cache.
        const GET_RATE_NOCACHE = 1 << 6;
    }
}
