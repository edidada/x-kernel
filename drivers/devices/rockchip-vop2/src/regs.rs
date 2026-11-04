// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VOP2 register map.
//!
//! Offsets are RK3588 TRM values, laid out so the compiler checks them: the
//! video-port and (E)smart window blocks are uniform arrays (0x100 and 0x200
//! stride), and the `const` assertions at the bottom fail the build if a
//! structure ever drifts from the hardware map.

use tock_registers::{
    register_bitfields, register_structs,
    registers::{ReadOnly, ReadWrite},
};

register_bitfields![u32,
    /// Video port display control.
    pub DSP_CTRL [
        /// Output interface mode.
        OUT_MODE OFFSET(0) NUMBITS(4) [],
        /// Post-dither down enable.
        PRE_DITHER_DOWN_EN OFFSET(16) NUMBITS(1) [],
        /// Holds the port in standby so window updates can be latched.
        STANDBY OFFSET(31) NUMBITS(1) [],
    ],

    /// Window region 0 control.
    pub REGION0_CTRL [
        /// Window enable.
        ENABLE OFFSET(0) NUMBITS(1) [],
        /// Framebuffer format. Must stay 0 (XRGB8888) until the driver learns
        /// to describe other layouts to the framebuffer layer.
        FORMAT OFFSET(1) NUMBITS(5) [],
        /// Dither up from a lower source depth.
        DITHER_UP OFFSET(12) NUMBITS(1) [],
        /// Red/blue channel swap.
        RB_SWAP OFFSET(14) NUMBITS(1) [],
    ],

    /// Global commit register.
    pub CFG_DONE [
        /// Video port 0 commit.
        VP0 OFFSET(0) NUMBITS(1) [],
        /// Video port 1 commit.
        VP1 OFFSET(1) NUMBITS(1) [],
        /// Video port 2 commit.
        VP2 OFFSET(2) NUMBITS(1) [],
        /// Video port 3 commit.
        VP3 OFFSET(3) NUMBITS(1) [],
        /// Commit enable shared by all video ports.
        GLB_EN OFFSET(15) NUMBITS(1) [],
        /// Write mask for the video port bits in 0..=15.
        VP_MASK OFFSET(16) NUMBITS(4) [],
    ],
];

register_structs! {
    /// One video port register block (0x100 bytes, one per port).
    pub VpRegisters {
        (0x00 => pub dsp_ctrl: ReadWrite<u32, DSP_CTRL::Register>),
        (0x04 => _reserved0),
        (0x34 => pub post_dsp_hact_info: ReadOnly<u32>),
        (0x38 => pub post_dsp_vact_info: ReadOnly<u32>),
        (0x3c => _reserved1),
        (0x100 => @END),
    }
}

register_structs! {
    /// One (E)smart window register block (0x200 bytes, one per port).
    pub WindowRegisters {
        (0x00 => _reserved0),
        (0x10 => pub region0_ctrl: ReadWrite<u32, REGION0_CTRL::Register>),
        (0x14 => pub yrgb_mst: ReadWrite<u32>),
        (0x18 => pub cbr_mst: ReadWrite<u32>),
        (0x1c => pub vir: ReadWrite<u32>),
        (0x20 => pub act_info: ReadWrite<u32>),
        (0x24 => pub dsp_info: ReadWrite<u32>),
        (0x28 => pub dsp_st: ReadWrite<u32>),
        (0x2c => _reserved1),
        (0x30 => pub scl_ctrl: ReadWrite<u32>),
        (0x34 => pub scl_factor_yrgb: ReadWrite<u32>),
        (0x38 => pub scl_factor_cbr: ReadWrite<u32>),
        (0x3c => _reserved2),
        (0x200 => @END),
    }
}

register_structs! {
    /// The controller's register window, as described by the firmware `reg`
    /// property (the first 0x2000 bytes of it are what this driver uses).
    pub Vop2Registers {
        (0x0000 => pub cfg_done: ReadWrite<u32, CFG_DONE::Register>),
        (0x0004 => pub version: ReadOnly<u32>),
        (0x0008 => pub auto_gating: ReadWrite<u32>),
        (0x000c => _reserved0),
        (0x0c00 => pub vp: [VpRegisters; 4]),
        (0x1000 => _reserved1),
        (0x1800 => pub win: [WindowRegisters; 4]),
        (0x2000 => @END),
    }
}

impl Vop2Registers {
    /// Video port `id` register block.
    pub(crate) fn vp(&self, id: u8) -> &VpRegisters {
        &self.vp[usize::from(id)]
    }

    /// (E)smart window block that carries video port `id`'s scan-out.
    pub(crate) fn window(&self, id: u8) -> &WindowRegisters {
        &self.win[usize::from(id)]
    }
}

/// Compile-time proof that the generated layout matches the TRM map.
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(size_of::<Vop2Registers>() == 0x2000);
    assert!(offset_of!(Vop2Registers, cfg_done) == 0x0000);
    assert!(offset_of!(Vop2Registers, version) == 0x0004);
    assert!(offset_of!(Vop2Registers, auto_gating) == 0x0008);
    assert!(offset_of!(Vop2Registers, vp) == 0x0c00);
    assert!(offset_of!(Vop2Registers, win) == 0x1800);

    assert!(size_of::<VpRegisters>() == 0x100);
    assert!(offset_of!(VpRegisters, dsp_ctrl) == 0x00);
    assert!(offset_of!(VpRegisters, post_dsp_hact_info) == 0x34);
    assert!(offset_of!(VpRegisters, post_dsp_vact_info) == 0x38);

    assert!(size_of::<WindowRegisters>() == 0x200);
    assert!(offset_of!(WindowRegisters, region0_ctrl) == 0x10);
    assert!(offset_of!(WindowRegisters, yrgb_mst) == 0x14);
    assert!(offset_of!(WindowRegisters, cbr_mst) == 0x18);
    assert!(offset_of!(WindowRegisters, vir) == 0x1c);
    assert!(offset_of!(WindowRegisters, act_info) == 0x20);
    assert!(offset_of!(WindowRegisters, dsp_info) == 0x24);
    assert!(offset_of!(WindowRegisters, dsp_st) == 0x28);
    assert!(offset_of!(WindowRegisters, scl_ctrl) == 0x30);
    assert!(offset_of!(WindowRegisters, scl_factor_yrgb) == 0x34);
    assert!(offset_of!(WindowRegisters, scl_factor_cbr) == 0x38);
};
