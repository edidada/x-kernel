// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU-local storage for the native NMI entry.

use core::sync::atomic::Ordering;

use kcpu_slot::{cpu_slot, cpu_slot_cell};
use x86_64::{VirtAddr, registers::model_specific::GsBase};

pub(super) const VECTOR: u8 = 2;
pub(super) const IST_INDEX: u16 = 0;
pub(super) const STACK_BYTES: usize = 64 * 1024;

#[repr(C, align(16))]
struct Stack {
    bytes: [u8; STACK_BYTES],
    kernel_gs: u64,
    #[cfg(unittest)]
    nested_deliveries: u64,
}

const _: () = assert!(core::mem::offset_of!(Stack, kernel_gs) == STACK_BYTES);

// SAFETY: the reset template is all zeroes and contains no ownership or
// self-references. Each CPU initializes its own permanent stack metadata.
unsafe impl kcpu_slot::StaticSlotValue for Stack {}

cpu_slot_cell! {
    static STACK: crate::x86_64::nmi::Stack = crate::x86_64::nmi::Stack {
        bytes: [0; crate::x86_64::nmi::STACK_BYTES],
        kernel_gs: 0,
        #[cfg(unittest)]
        nested_deliveries: 0,
    };
}

cpu_slot! {
    static DISPATCH_STATE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
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

const IDLE: u8 = 0;
const RUNNING: u8 = 1;
const LATCHED: u8 = 2;

pub(super) fn is_active() -> bool {
    DISPATCH_STATE.current_shared().load(Ordering::Relaxed) != IDLE
}

pub(super) fn begin_dispatch() -> bool {
    // These atomics serialize same-CPU exception/NMI nesting, not cross-CPU
    // publication. A nested arrival latches another pass for the outer scope.
    let state = DISPATCH_STATE.current_shared();
    if state
        .compare_exchange(IDLE, RUNNING, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        true
    } else {
        state.store(LATCHED, Ordering::Relaxed);
        #[cfg(unittest)]
        {
            let address = GsBase::read().as_u64() as usize
                + STACK::offset()
                + core::mem::offset_of!(Stack, nested_deliveries);
            // SAFETY: this nested dispatch has kernel GS installed and is
            // pinned with IRQs masked. The scalar is permanent test metadata;
            // only this CPU writes it, in nesting order, with no Rust borrow.
            unsafe { core::arch::asm!("inc qword ptr [{}]", in(reg) address, options(nostack)) };
        }
        false
    }
}

pub(super) fn finish_dispatch() -> bool {
    let state = DISPATCH_STATE.current_shared();
    if state
        .compare_exchange(RUNNING, IDLE, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        true
    } else {
        // An arrival before this store is covered by the imminent pass.
        state.store(RUNNING, Ordering::Relaxed);
        false
    }
}
