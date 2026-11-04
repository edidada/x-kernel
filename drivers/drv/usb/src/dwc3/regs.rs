// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DWC3 global register block (Synopsys databook §1.2 "Global Registers").
//!
//! The xHCI registers live at offset 0 of the same window; the global block
//! starts at `GLOBALS_START`, well above the xHCI region, which is why a single
//! mapping serves both and offsets must never be mixed up.

//! Offsets follow Linux `drivers/usb/dwc3/core.h` (DWC3_*): the block starts at
//! 0xc100, but the registers are NOT contiguous from there -- GCTL sits at 0xc110
//! and the per-PHY words start at 0xc200/0xc280/0xc2c0. Reading the wrong word
//! still returns a plausible value (0xc100 is GSBUSCFG0, 0xc140 is GHWPARAMS0),
//! which is exactly how this driver spent a bring-up round "matching the
//! reference" while never programming the USB2 PHY at all.

/// First global register (the block base, not the first register).
pub const GLOBALS_START: usize = 0xc100;

pub const GCTL: usize = GLOBALS_START + 0x10;
pub const GSTS: usize = GLOBALS_START + 0x18;
/// USB2 PHY configuration, one word per PHY, starting at 0xc200.
pub const GUSB2PHYCFG: usize = GLOBALS_START + 0x100;
pub const GUSB2PHYACC: usize = GLOBALS_START + 0x180;
/// SuperSpeed PIPE control, one word per PHY, starting at 0xc2c0.
pub const GUSB3PIPECTL: usize = GLOBALS_START + 0x1c0;
pub const GDCTL: usize = GLOBALS_START + 0x14;
/// Hardware parameters, 8 words from 0xc140.
pub const GHWPARAMS0: usize = GLOBALS_START + 0x40;
pub const GHWPARAMS1: usize = GLOBALS_START + 0x44;
pub const GHWPARAMS3: usize = GLOBALS_START + 0x4c;
pub const GHWPARAMS4: usize = GLOBALS_START + 0x50;
pub const DWC_USB3_GSNPSID: usize = GLOBALS_START + 0x20;

pub mod gctl {
    /// Core soft reset (bit 11); self-clears when complete. Bit 0 is
    /// DSBLCLKGTNG -- writing that instead, as this driver first did, resets
    /// nothing and the wait simply runs out.
    pub const CORESOFTRESET: u32 = 1 << 11;
    /// Port capability direction: host.
    pub const PRTCAPDIR_HOST: u32 = 1 << 12;
    pub const PRTCAPDIR_MASK: u32 = 0x3 << 12;
    /// SOF ITP sync (bit 10), not the scramble disable this used to claim.
    pub const SOFITPSYNC: u32 = 1 << 10;
}

pub mod gstst {
    /// Core is in reset.
    pub const CSRST: u32 = 1 << 0;
    /// Device is in host mode.
    pub const CURMOD_HOST: u32 = 1 << 29;
}

pub mod gux2phycfg {
    /// PHY interface: 0 = UTMI+ 8-bit, 1 = UTMI+16-bit (`utmi_wide`).
    pub const PHYIF: u32 = 1 << 3;
    /// Suspend USB2.0 PHY: 1 = suspended.
    pub const SUSPHY: u32 = 1 << 6;
    /// Disable the "enable sleep mode" handshake (`snps,dis_enblslpm_quirk`).
    pub const ENABLSLPM: u32 = 1 << 8;
}
