// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V signal frame layout and trampoline.
use kcpu::{GeneralRegisters, userspace::UserContext};
use posix_types::k_sigaltstack;

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
    /// Linux `user_regs_struct`: PC followed by x1..x31 (without x0).
    regs: [usize; 32],
    fpstate: [usize; 66],
}

const _: () = {
    assert!(size_of::<GeneralRegisters>() == 32 * size_of::<usize>());
    assert!(core::mem::offset_of!(GeneralRegisters, ra) == size_of::<usize>());
    assert!(core::mem::offset_of!(GeneralRegisters, t6) == 31 * size_of::<usize>());
    assert!(size_of::<MContext>() == 784);
};

impl MContext {
    /// Build machine context from a user context snapshot.
    pub fn new(uctx: &UserContext) -> Self {
        // SAFETY: GeneralRegisters is repr(C) with 32 initialized usize
        // fields in x0..x31 order; the size and boundary offsets are pinned.
        let raw_regs: [usize; 32] = unsafe { core::mem::transmute(uctx.regs) };
        let mut regs = [0; 32];
        regs[0] = uctx.sepc;
        regs[1..].copy_from_slice(&raw_regs[1..]);
        Self {
            regs,
            fpstate: [0; 66],
        }
    }

    /// Restore a user context from this machine context.
    pub fn restore(&self, uctx: &mut UserContext) {
        uctx.sepc = self.regs[0];
        let mut raw_regs = [0; 32];
        raw_regs[1..].copy_from_slice(&self.regs[1..]);
        // SAFETY: every array element is initialized; GeneralRegisters is
        // repr(C) with the same 32-word layout, and x0 stays zero.
        uctx.regs = unsafe { core::mem::transmute::<[usize; 32], GeneralRegisters>(raw_regs) };
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
    pub stack: k_sigaltstack,
    /// `uc_sigmask` restored into the thread blocked set at `sigreturn`.
    pub sigmask: SignalSet,
    __unused: [u8; 1024 / 8 - size_of::<SignalSet>()],
    abi_pad: [u8; 8],
    /// `uc_mcontext` general-purpose register snapshot.
    pub mcontext: MContext,
}

const _: () = {
    assert!(core::mem::offset_of!(UContext, mcontext) == 176);
    assert!(size_of::<UContext>() == 960);
};

impl UContext {
    /// Build a user context frame for signal handling.
    pub fn new(uctx: &UserContext, sigmask: SignalSet) -> Self {
        Self {
            flags: 0,
            link: 0,
            stack: SignalStack::default().into(),
            sigmask,
            __unused: [0; 1024 / 8 - size_of::<SignalSet>()],
            abi_pad: [0; 8],
            mcontext: MContext::new(uctx),
        }
    }
}
