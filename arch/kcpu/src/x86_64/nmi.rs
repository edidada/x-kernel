// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU-local storage for the native NMI entry.

use core::sync::atomic::Ordering;

use kcpu_slot::{cpu_slot, cpu_slot_cell};
use x86_64::{VirtAddr, registers::model_specific::GsBase};

pub(super) const VECTOR: u8 = 2;
pub(super) const IST_INDEX: u16 = 0;
const STACK_BYTES: usize = 64 * 1024;

#[repr(C, align(16))]
struct Stack {
    bytes: [u8; STACK_BYTES],
    kernel_gs: u64,
}

// SAFETY: the reset template is all zeroes and contains no ownership or
// self-references. Each CPU initializes its own permanent stack metadata.
unsafe impl kcpu_slot::StaticSlotValue for Stack {}

cpu_slot_cell! {
    static STACK: crate::x86_64::nmi::Stack = crate::x86_64::nmi::Stack {
        bytes: [0; crate::x86_64::nmi::STACK_BYTES],
        kernel_gs: 0,
    };
}

cpu_slot! {
    static IS_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
}

/// Prepares the aligned IST top on this CPU before its IDT is loaded.
/// Assembly reads the trusted GS value at this address on entry.
pub(super) fn prepare_stack() -> VirtAddr {
    let kernel_gs = GsBase::read().as_u64();
    let top = kernel_gs + (STACK::offset() + core::mem::offset_of!(Stack, kernel_gs)) as u64;
    // SAFETY: boot has installed this CPU's permanent slot area in GS.
    // The slot offset and field offset select an aligned u64 in that area.
    // No gate can use the stack until IDT load. Only metadata is written.
    unsafe { (top as *mut u64).write(kernel_gs) };
    VirtAddr::new(top)
}

pub(super) fn is_active() -> bool {
    IS_ACTIVE.current_shared().load(Ordering::Relaxed)
}

pub(super) fn set_active(active: bool) {
    IS_ACTIVE.current_shared().store(active, Ordering::Relaxed);
}
