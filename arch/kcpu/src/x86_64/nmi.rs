// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dedicated NMI stacks and their GS-independent entry metadata.

use kcpu_slot::{CpuLocalGuard, CpuPin, cpu_slot_cell};

const STACK_BYTES: usize = 64 * 1024;
pub(super) const IST_INDEX: u16 = 0;

// Layout consumed by excp.S after saving a complete ExceptionContext. The IST
// points just below metadata, so the entry can read it without a usable GS.
#[repr(C, align(16))]
struct NmiStack {
    bytes: [u8; STACK_BYTES],
    kernel_gs: u64,
    nested_top: u64,
    primary_top: u64,
    is_nested: u64,
    pending: u64,
    reserved: u64,
}

// excp.S accesses these fixed metadata fields only after saving the complete
// 23-word context. Make dependency/structure drift a compile failure.
const _: () = {
    assert!(core::mem::size_of::<super::ExceptionContext>() == 23 * 8);
    assert!(core::mem::offset_of!(NmiStack, kernel_gs) == STACK_BYTES);
    assert!(core::mem::offset_of!(NmiStack, nested_top) == STACK_BYTES + 8);
    assert!(core::mem::offset_of!(NmiStack, primary_top) == STACK_BYTES + 16);
    assert!(core::mem::offset_of!(NmiStack, is_nested) == STACK_BYTES + 24);
    assert!(core::mem::offset_of!(NmiStack, pending) == STACK_BYTES + 32);
    assert!(
        core::mem::offset_of!(
            x86_64::structures::tss::TaskStateSegment,
            interrupt_stack_table
        ) == 36
    );
};

cpu_slot_cell! {
    static PRIMARY: crate::x86_64::nmi::NmiStack = crate::x86_64::nmi::NmiStack::new();
}
cpu_slot_cell! {
    static NESTED: crate::x86_64::nmi::NmiStack = crate::x86_64::nmi::NmiStack::new();
}

// SAFETY: reset templates contain only zeroed bytes and integer metadata;
// each CPU receives independent storage and initialization installs local pointers.
unsafe impl kcpu_slot::StaticSlotValue for NmiStack {}

impl NmiStack {
    const fn new() -> Self {
        Self {
            bytes: [0; STACK_BYTES],
            kernel_gs: 0,
            nested_top: 0,
            primary_top: 0,
            is_nested: 0,
            pending: 0,
            reserved: 0,
        }
    }

    fn top(&self) -> u64 {
        core::ptr::addr_of!(self.kernel_gs) as u64
    }
}

/// Initializes this CPU's two permanent stacks before loading its NMI IDT gate.
pub(super) fn init() -> x86_64::VirtAddr {
    let pin = startup_pin();
    let kernel_gs = pin.base() as u64;
    // SAFETY: CPU bring-up is exclusive, before the NMI IDT is installed. Both
    // slots are permanent and no exception can observe partially set metadata.
    let primary = unsafe { PRIMARY.get_mut_unchecked(&pin) };
    // SAFETY: same pre-IDT exclusive initialization of the independent slot.
    let nested = unsafe { NESTED.get_mut_unchecked(&pin) };
    let first = primary.top();
    let second = nested.top();
    for (stack, is_nested) in [(primary, 0), (nested, 1)] {
        stack.kernel_gs = kernel_gs;
        stack.nested_top = second;
        stack.primary_top = first;
        stack.is_nested = is_nested;
    }
    x86_64::VirtAddr::new(first)
}

// Boot already excludes IRQs/migration. Reading CpuPin::base needs no kernel
// scheduler guard and must not depend on a current task during CPU bring-up.
fn startup_pin() -> impl CpuLocalGuard {
    struct BootPin(core::marker::PhantomData<*mut ()>);
    // SAFETY: private value is created only in pinned pre-scheduler CPU setup.
    unsafe impl CpuPin for BootPin {
        fn current_cpu(&self) -> kcpu_slot::CpuId {
            kiface_cpu_api::CurrentCpuIf::current_cpu()
        }
    }
    // SAFETY: pre-IDT CPU setup excludes ordinary IRQs and all competing local
    // accesses; this private token cannot leave the exclusive startup scope.
    unsafe impl CpuLocalGuard for BootPin {}
    BootPin(core::marker::PhantomData)
}

kcpu_slot::cpu_slot! {
    static IS_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
}

pub(super) fn is_active() -> bool {
    IS_ACTIVE
        .current_shared()
        .load(core::sync::atomic::Ordering::Relaxed)
}

pub(super) fn set_active(active: bool) {
    IS_ACTIVE
        .current_shared()
        .store(active, core::sync::atomic::Ordering::Relaxed);
}
