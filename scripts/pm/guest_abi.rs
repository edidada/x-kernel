// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux syscall and ELF entry ABIs for the standalone PM guest probe.

use core::arch::{asm, global_asm};

#[cfg(target_arch = "x86_64")]
pub const ARCH: &str = "x86_64";
#[cfg(target_arch = "aarch64")]
pub const ARCH: &str = "aarch64";
#[cfg(target_arch = "riscv64")]
pub const ARCH: &str = "riscv64";

#[cfg(target_arch = "x86_64")]
global_asm!(
    ".section .text",
    ".global _start",
    ".type _start,@function",
    "_start:",
    "and rsp, -16",
    "call e2e_main",
    "ud2",
    ".size _start, .-_start"
);
#[cfg(target_arch = "aarch64")]
global_asm!(
    ".section .text",
    ".global _start",
    ".type _start,%function",
    "_start:",
    "bl e2e_main",
    "brk #0",
    ".size _start, .-_start"
);
#[cfg(target_arch = "riscv64")]
global_asm!(
    ".section .text",
    ".global _start",
    ".type _start,@function",
    "_start:",
    "andi sp, sp, -16",
    "call e2e_main",
    "unimp",
    ".size _start, .-_start"
);

#[cfg(target_arch = "x86_64")]
pub mod nr {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const CLOSE: u64 = 3;
    pub const SEEK: u64 = 8;
    pub const MMAP: u64 = 9;
    pub const YIELD: u64 = 24;
    pub const SLEEP: u64 = 35;
    pub const EXIT: u64 = 60;
    pub const WAIT: u64 = 61;
    pub const FSYNC: u64 = 74;
    pub const REBOOT: u64 = 169;
    pub const AFFINITY: u64 = 203;
    pub const CLOCK: u64 = 228;
    pub const OPEN: u64 = 257;
    pub const TIMER_CREATE: u64 = 283;
    pub const TIMER_SET: u64 = 286;
}

#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
pub mod nr {
    pub const READ: u64 = 63;
    pub const WRITE: u64 = 64;
    pub const CLOSE: u64 = 57;
    pub const SEEK: u64 = 62;
    pub const MMAP: u64 = 222;
    pub const YIELD: u64 = 124;
    pub const SLEEP: u64 = 101;
    pub const EXIT: u64 = 93;
    pub const WAIT: u64 = 260;
    pub const FSYNC: u64 = 82;
    pub const REBOOT: u64 = 142;
    pub const AFFINITY: u64 = 122;
    pub const CLOCK: u64 = 113;
    pub const OPEN: u64 = 56;
    pub const TIMER_CREATE: u64 = 85;
    pub const TIMER_SET: u64 = 86;
}

/// Invokes the target Linux syscall ABI, returning its signed raw result.
///
/// # Safety
/// Pointer arguments must satisfy the selected syscall's initialization,
/// extent, lifetime and aliasing requirements. The entry runs on a live user
/// stack. Calls cannot retain borrowed pointers beyond their valid lifetime.
pub unsafe fn call(n: u64, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> i64 {
    let result: i64;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: caller owns syscall memory contracts; rcx/r11 are clobbered by
    // SYSCALL. All other registers follow the Linux x86-64 syscall convention.
    unsafe {
        asm!("syscall", inlateout("rax") n => result, in("rdi") a,
            in("rsi") b, in("rdx") c, in("r10") d, in("r8") e, in("r9") f,
            lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: caller owns syscall memory contracts; Linux preserves registers
    // other than the result in x0 across SVC, including the user stack pointer.
    unsafe {
        asm!("svc #0", in("x8") n, inlateout("x0") a => result,
            in("x1") b, in("x2") c, in("x3") d, in("x4") e, in("x5") f,
            options(nostack));
    }
    #[cfg(target_arch = "riscv64")]
    // SAFETY: caller owns syscall memory contracts; Linux returns in a0 and
    // preserves the remaining userspace register context across ECALL.
    unsafe {
        asm!("ecall", in("a7") n, inlateout("a0") a => result,
            in("a1") b, in("a2") c, in("a3") d, in("a4") e, in("a5") f,
            options(nostack));
    }
    result
}

/// Creates a separate process retaining the copied stack and shared mappings.
pub fn fork() -> i64 {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: fork has no pointer operands and copies the live user stack.
    unsafe {
        call(57, 0, 0, 0, 0, 0, 0)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    // SAFETY: clone with only SIGCHLD and a null new stack has fork semantics;
    // no CLONE_VM/TID/TLS flags are set and all optional pointers are null.
    unsafe {
        call(220, 17, 0, 0, 0, 0, 0)
    }
}
