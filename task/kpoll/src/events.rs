// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use bitflags::bitflags;

bitflags! {
    /// I/O events.
    #[derive(Debug, Clone, Copy)]
    pub struct IoEvents: u32 {
        /// Available for read.
        const IN = 0x0001;
        /// Urgent data for read.
        const PRI = 0x0002;
        /// Available for write.
        const OUT = 0x0004;
        /// Error condition.
        const ERR = 0x0008;
        /// Hang up.
        const HUP = 0x0010;
        /// Invalid request.
        const NVAL = 0x0020;
        /// Equivalent to [`IN`](Self::IN).
        const RDNORM = 0x0040;
        /// Priority band data can be read.
        const RDBAND = 0x0080;
        /// Equivalent to [`OUT`](Self::OUT).
        const WRNORM = 0x0100;
        /// Priority data can be written.
        const WRBAND = 0x0200;
        /// Message.
        const MSG = 0x0400;
        /// Remove.
        const REMOVE = 0x1000;
        /// Stream socket peer closed its write half.
        const RDHUP = 0x2000;
        /// Events reported even when callers did not request them.
        const ALWAYS_POLL = Self::ERR.bits() | Self::HUP.bits();
    }
}
