// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cross-compiled fixture for production offset and base-register instructions.
#![no_std]
#![allow(dead_code)]

#[path = "../src/macros.rs"]
mod production_macros;

unsafe extern "C" {
    static OFFSET_SMALL: u8;
    static OFFSET_BOUNDARY: u8;
    static OFFSET_ROUND: u8;
    static OFFSET_LIMIT: u8;
}

#[unsafe(no_mangle)]
pub fn offset_small() -> usize {
    __cpu_slot_symbol_offset!(OFFSET_SMALL)
}
#[unsafe(no_mangle)]
pub fn offset_boundary() -> usize {
    __cpu_slot_symbol_offset!(OFFSET_BOUNDARY)
}
#[cfg(target_arch = "aarch64")]
unsafe extern "C" {
    static OFFSET_WIDE: u8;
}

#[cfg(target_arch = "aarch64")]
#[unsafe(no_mangle)]
pub fn offset_wide() -> usize {
    __cpu_slot_symbol_offset!(OFFSET_WIDE)
}

#[unsafe(no_mangle)]
pub fn offset_round() -> usize {
    __cpu_slot_symbol_offset!(OFFSET_ROUND)
}
#[unsafe(no_mangle)]
pub fn offset_limit() -> usize {
    __cpu_slot_symbol_offset!(OFFSET_LIMIT)
}

#[path = "../src/arch.rs"]
mod arch;
#[path = "../src/layout.rs"]
mod layout;

#[unsafe(no_mangle)]
pub fn read_base() -> usize {
    arch::current_base()
}

/// # Safety
/// Same exclusive initialization contract as production set_current_base.
#[unsafe(no_mangle)]
pub unsafe fn write_base(value: usize) {
    // SAFETY: This wrapper preserves the production initialization contract;
    // it is cross-compiled and inspected, never executed in the host test.
    unsafe { arch::set_current_base(value) }
}
