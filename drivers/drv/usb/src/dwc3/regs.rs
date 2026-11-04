// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DWC3 global register block (Synopsys databook §1.2 "Global Registers").
//!
//! The xHCI registers live at offset 0 of the same window; the global block
//! starts at `GLOBALS_START`, well above the xHCI region, which is why a single
//! mapping serves both and offsets must never be mixed up.

/// First global register.
pub const GLOBALS_START: usize = 0xc100;

pub const GCTL: usize = GLOBALS_START;
pub const GSTS: usize = GLOBALS_START + 0x08;
pub const GUSB2PHYCFG: usize = GLOBALS_START + 0x40;
pub const GUSB2PHYACC: usize = GLOBALS_START + 0x48;
pub const GUSB3PIPECTL: usize = GLOBALS_START + 0x58;
pub const GDCTL: usize = GLOBALS_START + 0x14;
pub const GTXFIFOSIZ: usize = GLOBALS_START + 0x80;
pub const GRXFIFOSIZ: usize = GLOBALS_START + 0x100;
pub const GEVNTADR: usize = GLOBALS_START + 0x180;
pub const GEVNTSIZ: usize = GLOBALS_START + 0x188;
pub const GEVNTCOUNT: usize = GLOBALS_START + 0x18c;
pub const GHWPARAMS0: usize = GLOBALS_START + 0x40 + 0x38;
pub const GHWPARAMS1: usize = GLOBALS_START + 0x78;
pub const GHWPARAMS2: usize = GLOBALS_START + 0x7c;
pub const GHWPARAMS3: usize = GLOBALS_START + 0x80;
pub const GHWPARAMS4: usize = GLOBALS_START + 0x84;
pub const GHWPARAMS5: usize = GLOBALS_START + 0x88;
pub const GHWPARAMS6: usize = GLOBALS_START + 0x8c;
pub const GHWPARAMS7: usize = GLOBALS_START + 0x90;
pub const DWC_USB3_GSNPSID: usize = 0x00;

pub mod gctl {
    /// Core soft reset; self-clears when complete.
    pub const CORESOFTRESET: u32 = 1 << 0;
    /// Port capability direction: host.
    pub const PRTCAPDIR_HOST: u32 = 1 << 12;
    pub const PRTCAPDIR_MASK: u32 = 0x3 << 12;
    /// Disable the scrambling of the PIPE (SuperSpeed).
    pub const DISSCRAMBLE: u32 = 1 << 10;
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
