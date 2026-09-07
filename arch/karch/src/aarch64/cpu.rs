// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for AArch64.

/// Halt the current CPU terminally.
///
/// Masks every DAIF exception class — debug exceptions, SError, IRQs
/// (including PMR-mode pseudo-NMIs), and FIQs — and active hardware NMIs
/// via ALLINT, then repeatedly executes `wfi` until the CPU is reset.
/// A wake-up may end the wait instruction, but interrupt delivery is never
/// restored and execution stays in the terminal loop. This never returns.
#[inline]
pub fn stop_cpu() -> ! {
    crate::prepare_cpu_stop();
    loop {
        aarch64_cpu::asm::wfi();
    }
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn await_interrupts() {
    aarch64_cpu::asm::wfi();
}
