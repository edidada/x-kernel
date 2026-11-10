// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RK3588 USBDP PHY register tables, transcribed verbatim from the pinned
//! reference (`drivers/phy/rockchip/phy-rockchip-usbdp.c`, linux-7.1.8 and the
//! linux-jr 6.1 BSP — the two are byte-identical for these sequences).
//!
//! All offsets are PMA-relative (the PHY window's `+0x8000` block). These are
//! analog/PLL tuning words written as plain 32-bit stores, in order.

/// `(offset, value)` pair, one row of an init sequence.
pub(crate) struct RegSeq {
    pub offset: usize,
    pub value: u32,
}

/// `rk_udphy_init_sequence`: the analog front-end / PLL bring-up table.
pub(crate) const INIT_SEQUENCE: &[RegSeq] = &[
    RegSeq {
        offset: 0x0104,
        value: 0x44,
    },
    RegSeq {
        offset: 0x0234,
        value: 0xe8,
    },
    RegSeq {
        offset: 0x0248,
        value: 0x44,
    },
    RegSeq {
        offset: 0x028c,
        value: 0x18,
    },
    RegSeq {
        offset: 0x081c,
        value: 0xe5,
    },
    RegSeq {
        offset: 0x0878,
        value: 0x00,
    },
    RegSeq {
        offset: 0x0994,
        value: 0x1c,
    },
    RegSeq {
        offset: 0x0af0,
        value: 0x00,
    },
    RegSeq {
        offset: 0x181c,
        value: 0xe5,
    },
    RegSeq {
        offset: 0x1878,
        value: 0x00,
    },
    RegSeq {
        offset: 0x1994,
        value: 0x1c,
    },
    RegSeq {
        offset: 0x1af0,
        value: 0x00,
    },
    RegSeq {
        offset: 0x0428,
        value: 0x60,
    },
    RegSeq {
        offset: 0x0d58,
        value: 0x33,
    },
    RegSeq {
        offset: 0x1d58,
        value: 0x33,
    },
    RegSeq {
        offset: 0x0990,
        value: 0x74,
    },
    RegSeq {
        offset: 0x0d64,
        value: 0x17,
    },
    RegSeq {
        offset: 0x08c8,
        value: 0x13,
    },
    RegSeq {
        offset: 0x1990,
        value: 0x74,
    },
    RegSeq {
        offset: 0x1d64,
        value: 0x17,
    },
    RegSeq {
        offset: 0x18c8,
        value: 0x13,
    },
    RegSeq {
        offset: 0x0d90,
        value: 0x40,
    },
    RegSeq {
        offset: 0x0da8,
        value: 0x40,
    },
    RegSeq {
        offset: 0x0dc0,
        value: 0x40,
    },
    RegSeq {
        offset: 0x0dd8,
        value: 0x40,
    },
    RegSeq {
        offset: 0x1d90,
        value: 0x40,
    },
    RegSeq {
        offset: 0x1da8,
        value: 0x40,
    },
    RegSeq {
        offset: 0x1dc0,
        value: 0x40,
    },
    RegSeq {
        offset: 0x1dd8,
        value: 0x40,
    },
    RegSeq {
        offset: 0x03c0,
        value: 0x30,
    },
    RegSeq {
        offset: 0x03c4,
        value: 0x06,
    },
    RegSeq {
        offset: 0x0e10,
        value: 0x00,
    },
    RegSeq {
        offset: 0x1e10,
        value: 0x00,
    },
    RegSeq {
        offset: 0x043c,
        value: 0x0f,
    },
    RegSeq {
        offset: 0x0d2c,
        value: 0xff,
    },
    RegSeq {
        offset: 0x1d2c,
        value: 0xff,
    },
    RegSeq {
        offset: 0x0d34,
        value: 0x0f,
    },
    RegSeq {
        offset: 0x1d34,
        value: 0x0f,
    },
    RegSeq {
        offset: 0x08fc,
        value: 0x2a,
    },
    RegSeq {
        offset: 0x0914,
        value: 0x28,
    },
    RegSeq {
        offset: 0x0a30,
        value: 0x03,
    },
    RegSeq {
        offset: 0x0e38,
        value: 0x03,
    },
    RegSeq {
        offset: 0x0ecc,
        value: 0x27,
    },
    RegSeq {
        offset: 0x0ed0,
        value: 0x22,
    },
    RegSeq {
        offset: 0x0ed4,
        value: 0x26,
    },
    RegSeq {
        offset: 0x18fc,
        value: 0x2a,
    },
    RegSeq {
        offset: 0x1914,
        value: 0x28,
    },
    RegSeq {
        offset: 0x1a30,
        value: 0x03,
    },
    RegSeq {
        offset: 0x1e38,
        value: 0x03,
    },
    RegSeq {
        offset: 0x1ecc,
        value: 0x27,
    },
    RegSeq {
        offset: 0x1ed0,
        value: 0x22,
    },
    RegSeq {
        offset: 0x1ed4,
        value: 0x26,
    },
    RegSeq {
        offset: 0x0048,
        value: 0x0f,
    },
    RegSeq {
        offset: 0x0060,
        value: 0x3c,
    },
    RegSeq {
        offset: 0x0064,
        value: 0xf7,
    },
    RegSeq {
        offset: 0x006c,
        value: 0x20,
    },
    RegSeq {
        offset: 0x0070,
        value: 0x7d,
    },
    RegSeq {
        offset: 0x0074,
        value: 0x68,
    },
    RegSeq {
        offset: 0x0af4,
        value: 0x1a,
    },
    RegSeq {
        offset: 0x1af4,
        value: 0x1a,
    },
    RegSeq {
        offset: 0x0440,
        value: 0x3f,
    },
    RegSeq {
        offset: 0x10d4,
        value: 0x08,
    },
    RegSeq {
        offset: 0x20d4,
        value: 0x08,
    },
    RegSeq {
        offset: 0x00d4,
        value: 0x30,
    },
    RegSeq {
        offset: 0x0024,
        value: 0x6e,
    },
];

/// `rk_udphy_24m_refclk_cfg`: the reference-clock tuning table for a 24 MHz
/// `refclk`. (26 MHz has a table too; the board uses 24 MHz, which is the only
/// rate `refclk_set` accepts here — anything else is a hard error.)
pub(crate) const REFCLK_24M_CFG: &[RegSeq] = &[
    RegSeq {
        offset: 0x0090,
        value: 0x68,
    },
    RegSeq {
        offset: 0x0094,
        value: 0x68,
    },
    RegSeq {
        offset: 0x0128,
        value: 0x24,
    },
    RegSeq {
        offset: 0x012c,
        value: 0x44,
    },
    RegSeq {
        offset: 0x0130,
        value: 0x3f,
    },
    RegSeq {
        offset: 0x0134,
        value: 0x44,
    },
    RegSeq {
        offset: 0x015c,
        value: 0xa9,
    },
    RegSeq {
        offset: 0x0160,
        value: 0x71,
    },
    RegSeq {
        offset: 0x0164,
        value: 0x71,
    },
    RegSeq {
        offset: 0x0168,
        value: 0xa9,
    },
    RegSeq {
        offset: 0x0174,
        value: 0xa9,
    },
    RegSeq {
        offset: 0x0178,
        value: 0x71,
    },
    RegSeq {
        offset: 0x017c,
        value: 0x71,
    },
    RegSeq {
        offset: 0x0180,
        value: 0xa9,
    },
    RegSeq {
        offset: 0x018c,
        value: 0x41,
    },
    RegSeq {
        offset: 0x0190,
        value: 0x00,
    },
    RegSeq {
        offset: 0x0194,
        value: 0x05,
    },
    RegSeq {
        offset: 0x01ac,
        value: 0x2a,
    },
    RegSeq {
        offset: 0x01b0,
        value: 0x17,
    },
    RegSeq {
        offset: 0x01b4,
        value: 0x17,
    },
    RegSeq {
        offset: 0x01b8,
        value: 0x2a,
    },
    RegSeq {
        offset: 0x01c8,
        value: 0x04,
    },
    RegSeq {
        offset: 0x01cc,
        value: 0x08,
    },
    RegSeq {
        offset: 0x01d0,
        value: 0x08,
    },
    RegSeq {
        offset: 0x01d4,
        value: 0x04,
    },
    RegSeq {
        offset: 0x01d8,
        value: 0x20,
    },
    RegSeq {
        offset: 0x01dc,
        value: 0x01,
    },
    RegSeq {
        offset: 0x01e0,
        value: 0x09,
    },
    RegSeq {
        offset: 0x01e4,
        value: 0x03,
    },
    RegSeq {
        offset: 0x01f0,
        value: 0x29,
    },
    RegSeq {
        offset: 0x01f4,
        value: 0x02,
    },
    RegSeq {
        offset: 0x01f8,
        value: 0x02,
    },
    RegSeq {
        offset: 0x01fc,
        value: 0x29,
    },
    RegSeq {
        offset: 0x0208,
        value: 0x2a,
    },
    RegSeq {
        offset: 0x020c,
        value: 0x17,
    },
    RegSeq {
        offset: 0x0210,
        value: 0x17,
    },
    RegSeq {
        offset: 0x0214,
        value: 0x2a,
    },
    RegSeq {
        offset: 0x0224,
        value: 0x20,
    },
    RegSeq {
        offset: 0x03f0,
        value: 0x0a,
    },
    RegSeq {
        offset: 0x03f4,
        value: 0x07,
    },
    RegSeq {
        offset: 0x03f8,
        value: 0x07,
    },
    RegSeq {
        offset: 0x03fc,
        value: 0x0c,
    },
    RegSeq {
        offset: 0x0404,
        value: 0x12,
    },
    RegSeq {
        offset: 0x0408,
        value: 0x1a,
    },
    RegSeq {
        offset: 0x040c,
        value: 0x1a,
    },
    RegSeq {
        offset: 0x0410,
        value: 0x3f,
    },
    RegSeq {
        offset: 0x0ce0,
        value: 0x68,
    },
    RegSeq {
        offset: 0x0ce8,
        value: 0xd0,
    },
    RegSeq {
        offset: 0x0cf0,
        value: 0x87,
    },
    RegSeq {
        offset: 0x0cf8,
        value: 0x70,
    },
    RegSeq {
        offset: 0x0d00,
        value: 0x70,
    },
    RegSeq {
        offset: 0x0d08,
        value: 0xa9,
    },
    RegSeq {
        offset: 0x1ce0,
        value: 0x68,
    },
    RegSeq {
        offset: 0x1ce8,
        value: 0xd0,
    },
    RegSeq {
        offset: 0x1cf0,
        value: 0x87,
    },
    RegSeq {
        offset: 0x1cf8,
        value: 0x70,
    },
    RegSeq {
        offset: 0x1d00,
        value: 0x70,
    },
    RegSeq {
        offset: 0x1d08,
        value: 0xa9,
    },
    RegSeq {
        offset: 0x0a3c,
        value: 0xd0,
    },
    RegSeq {
        offset: 0x0a44,
        value: 0xd0,
    },
    RegSeq {
        offset: 0x0a48,
        value: 0x01,
    },
    RegSeq {
        offset: 0x0a4c,
        value: 0x0d,
    },
    RegSeq {
        offset: 0x0a54,
        value: 0xe0,
    },
    RegSeq {
        offset: 0x0a5c,
        value: 0xe0,
    },
    RegSeq {
        offset: 0x0a64,
        value: 0xa8,
    },
    RegSeq {
        offset: 0x1a3c,
        value: 0xd0,
    },
    RegSeq {
        offset: 0x1a44,
        value: 0xd0,
    },
    RegSeq {
        offset: 0x1a48,
        value: 0x01,
    },
    RegSeq {
        offset: 0x1a4c,
        value: 0x0d,
    },
    RegSeq {
        offset: 0x1a54,
        value: 0xe0,
    },
    RegSeq {
        offset: 0x1a5c,
        value: 0xe0,
    },
    RegSeq {
        offset: 0x1a64,
        value: 0xa8,
    },
    RegSeq {
        offset: 0x00d4,
        value: 0x30,
    },
];
