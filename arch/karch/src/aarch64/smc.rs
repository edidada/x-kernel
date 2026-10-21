// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Secure Monitor Call (SMC) — the ARM SMC Calling Convention (SMCCC)
//! SMC32/SMC64 fast-call entry for firmware services (SCMI, PSCI).
//!
//! A fast call returns once the firmware handler completes; there is no
//! scheduler re-entry, so a plain synchronous wrapper is the whole API.

/// Issue an SMC with four argument registers, returning x0..x3.
///
/// The caller picks the function id (SMCCC function identifier) in `x0`;
/// clobbers are exactly x0-x3 plus the architectural x4-x17 scratch set.
#[inline]
pub fn smc_call(x0: u64, x1: u64, x2: u64, x3: u64) -> [u64; 4] {
    let mut ret = [0u64; 4];
    // SAFETY: inline asm executes the architectural SMC instruction with
    // the SMCCC register contract; all registers outside x0-x3 are
    // declared clobbered and the result is written back from x0-x3.
    unsafe {
        core::arch::asm!(
            "smc #0",
            inlateout("x0") x0 => ret[0],
            inlateout("x1") x1 => ret[1],
            inlateout("x2") x2 => ret[2],
            inlateout("x3") x3 => ret[3],
            lateout("x4") _,
            lateout("x5") _,
            lateout("x6") _,
            lateout("x7") _,
            lateout("x8") _,
            lateout("x9") _,
            lateout("x10") _,
            lateout("x11") _,
            lateout("x12") _,
            lateout("x13") _,
            lateout("x14") _,
            lateout("x15") _,
            lateout("x16") _,
            lateout("x17") _,
            options(nostack)
        );
    }
    ret
}
