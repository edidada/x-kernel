// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn linux_riscv64_signal_layout_has_pc_then_x1_through_x31() {
    // Linux UAPI user_regs_struct and glibc gregset_t contain 32 words:
    // PC, RA, SP, GP, TP, ... T6. x0 is never a userspace frame slot.
    assert_eq!(size_of::<GeneralRegisters>(), 31 * 8);
    assert_eq!(core::mem::offset_of!(MContext, pc), 0);
    assert_eq!(core::mem::offset_of!(MContext, regs), 8);
    assert_eq!(core::mem::offset_of!(GeneralRegisters, ra), 0);
    assert_eq!(core::mem::offset_of!(GeneralRegisters, sp), 8);
    assert_eq!(core::mem::offset_of!(GeneralRegisters, a0), 9 * 8);
    assert_eq!(core::mem::offset_of!(MContext, fpstate), 32 * 8);
    assert_eq!(size_of::<MContext>(), 784);
    assert_eq!(core::mem::offset_of!(UContext, mcontext), 176);
    assert_eq!(size_of::<UContext>(), 960);
}

#[def_test]
fn signal_handler_register_edits_restore_without_an_x0_shift() {
    let mut user = UserContext::new(0x1000, memaddr::VirtAddr::from_usize(0x8000), 10);
    user.regs.ra = 0x1200;
    user.regs.tp = 0x9000;
    user.regs.s11 = 0x1111;
    let mut frame = MContext::new(&user);
    assert_eq!(frame.regs.ra, 0x1200);
    assert_eq!(frame.regs.sp, 0x8000);
    frame.pc = 0x2000;
    frame.regs.a0 = 42;
    frame.regs.ra = 0x2200;
    frame.restore(&mut user);
    assert_eq!(user.sepc, 0x2000);
    assert_eq!(user.regs.a0, 42);
    assert_eq!(user.regs.ra, 0x2200);
    assert_eq!(user.regs.sp, 0x8000);
    assert_eq!(user.regs.tp, 0x9000);
    assert_eq!(user.regs.s11, 0x1111);
}
