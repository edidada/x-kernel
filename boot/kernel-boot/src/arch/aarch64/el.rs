// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 exception level switching for early boot.

use aarch64_cpu::{asm::barrier, registers::*};

/// Initialize EL2 with VHE (Virtualization Host Extensions).
///
/// Instead of dropping to EL1, the kernel stays in EL2 with `HCR_EL2.E2H=1`.
/// VHE hardware aliases `_EL1` register names to their EL2 counterparts,
/// so all subsequent kernel code operates transparently at EL2.
///
/// # Safety
///
/// Must run during early boot while the CPU is in EL2 with the expected boot register state. The
/// caller must ensure VHE is supported by the CPU (ARMv8.1+).
#[unsafe(link_section = ".idmap.text")]
pub unsafe fn init_el2_vhe() {
    let current_el = CurrentEL.read(CurrentEL::EL);
    assert_eq!(current_el, 2, "xhyper must be entered at EL2");

    // Stay in EL2 with VHE: E2H=1, RW=1, TGE=1
    HCR_EL2.write(HCR_EL2::E2H::SET + HCR_EL2::RW::EL1IsAarch64 + HCR_EL2::TGE::SET);
    barrier::isb(barrier::SY);

    // Clear CPTR_EL2.TFP (bit 10) to allow FP/NEON.
    // The aarch64-cpu crate does not define TFP, so we use raw bit ops.
    let cptr = CPTR_EL2.get();
    CPTR_EL2.set(cptr & !(1 << 10));

    // VHE-mode CNTHCTL_EL2 bit layout differs from non-VHE:
    //   bit 0  = EL0PCTEN  (don't trap EL0 CNTPCT reads)
    //   bit 1  = EL0VCTEN  (don't trap EL0 CNTVCT reads)
    //   bit 10 = EL1PTEN   (don't trap EL1 physical timer access)
    // SAFETY: raw MSR for VHE-specific bit layout not modeled by the crate.
    unsafe {
        core::arch::asm!(
            "mov {tmp}, #(1 | (1 << 1) | (1 << 10))",
            "msr cnthctl_el2, {tmp}",
            "isb",
            tmp = out(reg) _,
        );
    }

    CNTVOFF_EL2.set(0);
}
