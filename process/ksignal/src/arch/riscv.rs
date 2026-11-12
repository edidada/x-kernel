// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V signal frame layout and trampoline.
use kcpu::userspace::UserContext;

use crate::{SignalSet, SignalStack};

/// Stack alignment required when entering a user signal handler.
pub(crate) const SIGNAL_FRAME_ALIGN: usize = 16;

core::arch::global_asm!(
    "
.section .text
.balign 4096
.global signal_trampoline
signal_trampoline:
    li a7, 139
    ecall

.fill 4096 - (. - signal_trampoline), 1, 0
"
);

#[repr(C, align(16))]
#[derive(Clone)]
/// Machine context in the Linux RISC-V `mcontext_t` ABI.
pub struct MContext {
    /// Linux `user_regs_struct`: PC followed by registers x1 through x31.
    regs: [usize; 32],
    fpstate: [usize; 66],
}

impl MContext {
    /// Build machine context from a user context snapshot.
    pub fn new(uctx: &UserContext) -> Self {
        Self {
            regs: [
                uctx.sepc,
                uctx.regs.ra,
                uctx.regs.sp,
                uctx.regs.gp,
                uctx.regs.tp,
                uctx.regs.t0,
                uctx.regs.t1,
                uctx.regs.t2,
                uctx.regs.s0,
                uctx.regs.s1,
                uctx.regs.a0,
                uctx.regs.a1,
                uctx.regs.a2,
                uctx.regs.a3,
                uctx.regs.a4,
                uctx.regs.a5,
                uctx.regs.a6,
                uctx.regs.a7,
                uctx.regs.s2,
                uctx.regs.s3,
                uctx.regs.s4,
                uctx.regs.s5,
                uctx.regs.s6,
                uctx.regs.s7,
                uctx.regs.s8,
                uctx.regs.s9,
                uctx.regs.s10,
                uctx.regs.s11,
                uctx.regs.t3,
                uctx.regs.t4,
                uctx.regs.t5,
                uctx.regs.t6,
            ],
            fpstate: [0; 66],
        }
    }

    /// Restore a user context from this machine context.
    pub fn restore(&self, uctx: &mut UserContext) {
        uctx.sepc = self.regs[0];
        uctx.regs.ra = self.regs[1];
        uctx.regs.sp = self.regs[2];
        uctx.regs.gp = self.regs[3];
        uctx.regs.tp = self.regs[4];
        uctx.regs.t0 = self.regs[5];
        uctx.regs.t1 = self.regs[6];
        uctx.regs.t2 = self.regs[7];
        uctx.regs.s0 = self.regs[8];
        uctx.regs.s1 = self.regs[9];
        uctx.regs.a0 = self.regs[10];
        uctx.regs.a1 = self.regs[11];
        uctx.regs.a2 = self.regs[12];
        uctx.regs.a3 = self.regs[13];
        uctx.regs.a4 = self.regs[14];
        uctx.regs.a5 = self.regs[15];
        uctx.regs.a6 = self.regs[16];
        uctx.regs.a7 = self.regs[17];
        uctx.regs.s2 = self.regs[18];
        uctx.regs.s3 = self.regs[19];
        uctx.regs.s4 = self.regs[20];
        uctx.regs.s5 = self.regs[21];
        uctx.regs.s6 = self.regs[22];
        uctx.regs.s7 = self.regs[23];
        uctx.regs.s8 = self.regs[24];
        uctx.regs.s9 = self.regs[25];
        uctx.regs.s10 = self.regs[26];
        uctx.regs.s11 = self.regs[27];
        uctx.regs.t3 = self.regs[28];
        uctx.regs.t4 = self.regs[29];
        uctx.regs.t5 = self.regs[30];
        uctx.regs.t6 = self.regs[31];
    }
}

/// User-visible `ucontext_t` frame for RISC-V signal handlers.
#[repr(C)]
#[derive(Clone)]
pub struct UContext {
    /// `uc_flags`; written as zero by the kernel.
    pub flags: usize,
    /// `uc_link`; not used by signal frames and written as zero.
    pub link: usize,
    /// `uc_stack` snapshot of the alternate stack state.
    pub stack: SignalStack,
    /// `uc_sigmask` restored into the thread blocked set at `sigreturn`.
    pub sigmask: SignalSet,
    __unused: [u8; 1024 / 8 - size_of::<SignalSet>()],
    /// `uc_mcontext` general-purpose register snapshot.
    pub mcontext: MContext,
}

impl UContext {
    /// Build a user context frame for signal handling.
    pub fn new(uctx: &UserContext, sigmask: SignalSet) -> Self {
        Self {
            flags: 0,
            link: 0,
            stack: SignalStack::default(),
            sigmask,
            __unused: [0; 1024 / 8 - size_of::<SignalSet>()],
            mcontext: MContext::new(uctx),
        }
    }
}
