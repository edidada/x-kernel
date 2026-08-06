// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for AArch64.

use aarch64_cpu::registers::{CurrentEL, Readable};

use super::irq::disable_local_irq;

/// Returns the current AArch64 exception level.
#[inline]
pub fn current_exception_level() -> u8 {
    CurrentEL.read(CurrentEL::EL) as u8
}

/// Halt the current CPU.
///
/// Disables interrupts then executes WFI. Since interrupts are disabled,
/// this should stop execution until reset.
#[inline]
pub fn stop_cpu() {
    disable_local_irq();
    aarch64_cpu::asm::wfi();
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn await_interrupts() {
    aarch64_cpu::asm::wfi();
}
