// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 CPU context and trap support.

mod ctx;

pub mod instrs;

mod excp;

pub use self::ctx::{ExceptionContext as TrapFrame, ExceptionContext, FpState, TaskContext};

/// Initializes trap handling on the current CPU.
///
/// In detail, it initializes the exception vector, and sets `TTBR0_EL1` to 0 to
/// block low address access.
pub fn init_trap() {
    unsafe extern "C" {
        fn exception_vector_base();
    }
    // SAFETY: `exception_vector_base` is defined in `excp.S` and aligned per ARM vector
    // table requirements. Writing zero to TTBR0_EL1 disables low-address translation.
    unsafe {
        karch::write_trap_vector_base(exception_vector_base as *const () as usize);
        karch::write_user_page_table(memaddr::PhysAddr::from(0usize).into());
    }
}
