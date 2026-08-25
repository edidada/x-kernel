// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raw monotonic counter reads used to timestamp records.
//!
//! LCTrace deliberately does not go through `khal::time`. That path is an
//! inter-crate `kiface` call which then reads the configured timer mode before
//! touching the hardware, and `khal` itself carries LCTrace probes, so a
//! dependency on it would also close a cycle. Every supported architecture
//! exposes a free-running counter that a single instruction can read, so the
//! hot path reads it directly.
//!
//! The value is a raw tick count in the platform counter domain. Converting it
//! to wall time needs the counter frequency, which the snapshot caller injects
//! (see [`crate::write_snapshot`]).

/// Reads the platform's free-running monotonic counter.
#[inline(always)]
pub(crate) fn read_counter() -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        let ticks: u64;
        // SAFETY: `CNTPCT_EL0` is the architectural physical counter. It is
        // readable at EL1/EL2, the read has no side effects, and it clobbers
        // nothing beyond the destination register.
        unsafe { core::arch::asm!("mrs {}, cntpct_el0", out(reg) ticks, options(nomem, nostack)) };
        ticks
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: `rdtsc` is unconditionally available on every x86_64 CPU the
        // kernel supports, reads no memory, and has no side effects.
        unsafe { core::arch::x86_64::_rdtsc() }
    }
    #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
    {
        let ticks: u64;
        // SAFETY: `rdtime` reads the `time` CSR, which the SBI firmware leaves
        // readable from S-mode. The read has no side effects.
        unsafe { core::arch::asm!("rdtime {}", out(reg) ticks, options(nomem, nostack)) };
        ticks
    }
    #[cfg(target_arch = "loongarch64")]
    {
        let ticks: u64;
        // SAFETY: `rdtime.d` reads the stable counter into the first register
        // and its frequency id into the second; `$zero` discards the latter.
        // The read has no side effects.
        unsafe { core::arch::asm!("rdtime.d {}, $zero", out(reg) ticks, options(nomem, nostack)) };
        ticks
    }
}
