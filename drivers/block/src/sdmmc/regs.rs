// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Synopsys DesignWare Mobile Storage Host Controller (dw_mshc) register map.
//!
//! Register offsets and bit definitions are transcribed **verbatim** from the
//! upstream Linux driver `drivers/mmc/host/dw_mmc.h` (see
//! <https://github.com/torvalds/linux/blob/master/drivers/mmc/host/dw_mmc.h>).
//! These are the hardware truth for the RK3588 SD card controller at
//! `0xfe2c0000`; do not "simplify" or renumber them (skill rule 1: hardware is
//! truth).

// The full register map is transcribed verbatim; many symbols are reserved for
// future IDMAC / tuning / UHS paths and are intentionally kept unused for now.
#![allow(dead_code)]

/// Register offsets (SDMMC_*). Source: dw_mmc.h.
pub mod off {
    pub const CTRL: usize = 0x000;
    pub const PWREN: usize = 0x004;
    pub const CLKDIV: usize = 0x008;
    pub const CLKSRC: usize = 0x00c;
    pub const CLKENA: usize = 0x010;
    pub const TMOUT: usize = 0x014;
    pub const CTYPE: usize = 0x018;
    pub const BLKSIZ: usize = 0x01c;
    pub const BYTCNT: usize = 0x020;
    pub const INTMASK: usize = 0x024;
    pub const CMDARG: usize = 0x028;
    pub const CMD: usize = 0x02c;
    pub const RESP0: usize = 0x030;
    pub const RESP1: usize = 0x034;
    pub const RESP2: usize = 0x038;
    pub const RESP3: usize = 0x03c;
    pub const MINTSTS: usize = 0x040;
    pub const RINTSTS: usize = 0x044;
    pub const STATUS: usize = 0x048;
    pub const FIFOTH: usize = 0x04c;
    pub const CDETECT: usize = 0x050;
    pub const WRTPRT: usize = 0x054;
    pub const DEBNCE: usize = 0x064;
    pub const VERID: usize = 0x06c;
    pub const HCON: usize = 0x070;
    pub const UHS_REG: usize = 0x074;
    pub const RST_N: usize = 0x078;
    pub const BMOD: usize = 0x080;
    pub const PLDMND: usize = 0x084;
    pub const DBADDR: usize = 0x088;
    pub const IDSTS: usize = 0x08c;
    pub const IDINTEN: usize = 0x090;
    // Rockchip vendor tuning registers (sdhci-of-dwcmshc is unrelated; these are
    // for the dw_mshc IP). Source: dw_mmc-rockchip.c
    pub const TIMING_CON0: usize = 0x130;
    pub const TIMING_CON1: usize = 0x134;
    pub const MISC_CON: usize = 0x138;

    /// FIFO data register offset depends on controller version.
    /// < 2.40a: 0x100; >= 2.40a: 0x200. Source: dw_mmc.h (DATA_OFFSET/DATA_240A_OFFSET).
    pub const DATA_OFFSET: usize = 0x100;
    pub const DATA_240A_OFFSET: usize = 0x200;
}

/// CTRL register bits. Source: dw_mmc.h.
pub mod ctrl {
    pub const USE_IDMAC: u32 = 1 << 25;
    pub const DMA_ENABLE: u32 = 1 << 5;
    pub const INT_ENABLE: u32 = 1 << 4;
    pub const DMA_RESET: u32 = 1 << 2;
    pub const FIFO_RESET: u32 = 1 << 1;
    pub const RESET: u32 = 1 << 0;
    pub const ALL_RESET_FLAGS: u32 = RESET | FIFO_RESET | DMA_RESET;
}

/// CLKENA register bits. Source: dw_mmc.h.
pub mod clkena {
    pub const LOW_PWR: u32 = 1 << 16;
    pub const ENABLE: u32 = 1 << 0;
}

/// CTYPE register bits. Source: dw_mmc.h.
pub mod ctype {
    pub const CTYPE_8BIT: u32 = 1 << 16;
    pub const CTYPE_4BIT: u32 = 1 << 0;
    pub const CTYPE_1BIT: u32 = 0;
}

/// Interrupt status / mask bits. Source: dw_mmc.h.
pub mod int {
    pub const CD: u32 = 1 << 0;
    pub const RESP_ERR: u32 = 1 << 1;
    pub const CMD_DONE: u32 = 1 << 2;
    pub const DATA_OVER: u32 = 1 << 3;
    pub const TXDR: u32 = 1 << 4;
    pub const RXDR: u32 = 1 << 5;
    pub const RCRC: u32 = 1 << 6;
    pub const DCRC: u32 = 1 << 7;
    pub const RTO: u32 = 1 << 8;
    pub const DRTO: u32 = 1 << 9;
    pub const HTO: u32 = 1 << 10;
    pub const FRUN: u32 = 1 << 11;
    pub const HLE: u32 = 1 << 12;
    pub const SBE: u32 = 1 << 13;
    pub const ACD: u32 = 1 << 14;
    pub const EBE: u32 = 1 << 15;

    pub const DATA_ERROR_FLAGS: u32 = DRTO | DCRC | HTO | SBE | EBE | HLE;
    pub const CMD_ERROR_FLAGS: u32 = RTO | RCRC | RESP_ERR | HLE;
    pub const ERROR_FLAGS: u32 = DATA_ERROR_FLAGS | CMD_ERROR_FLAGS;
}

/// CMD register bits. Source: dw_mmc.h.
pub mod cmd {
    pub const START: u32 = 1 << 31;
    pub const USE_HOLD_REG: u32 = 1 << 29;
    pub const UPD_CLK: u32 = 1 << 21;
    pub const INIT: u32 = 1 << 15;
    pub const STOP: u32 = 1 << 14;
    pub const PRV_DAT_WAIT: u32 = 1 << 13;
    pub const SEND_STOP: u32 = 1 << 12;
    pub const DAT_WR: u32 = 1 << 10;
    pub const DAT_EXP: u32 = 1 << 9;
    pub const RESP_CRC: u32 = 1 << 8;
    pub const RESP_LONG: u32 = 1 << 7;
    pub const RESP_EXP: u32 = 1 << 6;

    #[inline]
    pub const fn indx(n: u32) -> u32 {
        n & 0x1f
    }
}

/// STATUS register helpers. Source: dw_mmc.h.
pub mod status {
    #[inline]
    pub const fn fcnt(x: u32) -> u32 {
        (x >> 17) & 0x1fff
    }
    pub const DMA_REQ: u32 = 1 << 31;
    pub const BUSY: u32 = 1 << 9;
    pub const FIFO_FULL: u32 = 1 << 3;
    pub const FIFO_EMPTY: u32 = 1 << 2;
}

/// FIFOTH register helper. Source: dw_mmc.h: SDMMC_SET_FIFOTH(m, r, t).
#[inline]
pub const fn set_fifoth(m: u32, r: u32, t: u32) -> u32 {
    ((m & 0x7) << 28) | ((r & 0xfff) << 16) | (t & 0xfff)
}

/// Read back the RX_WMark-encoded FIFO depth from a FIFOTH value. The power-on
/// FIFOTH encodes `FIFO_DEPTH - 1` in RX_WMark; source: dw_mmc.c probe.
#[inline]
pub const fn fifo_depth_from_fifoth(fifoth: u32) -> u32 {
    1 + ((fifoth >> 16) & 0xfff)
}

/// HCON field helpers. Source: dw_mmc.h.
pub mod hcon {
    #[inline]
    pub const fn trans_mode(x: u32) -> u32 {
        (x >> 16) & 0x3
    }
    #[inline]
    pub const fn hdata_width(x: u32) -> u32 {
        (x >> 7) & 0x7
    }
    /// HOST_DATA_WIDTH == 0 -> 16-bit, 1 -> 32-bit, 2 -> 64-bit.
    pub const DMA_INTERFACE_NODMA: u32 = 0x3;
}

/// VERID helpers. Source: dw_mmc.h.
#[inline]
pub const fn verid(x: u32) -> u32 {
    x & 0xffff
}
pub const DW_MMC_240A: u32 = 0x240a;

/// RST_N hardware-reset register bit. Source: dw_mmc.h.
pub mod rst {
    pub const HWACTIVE: u32 = 0x1;
}
