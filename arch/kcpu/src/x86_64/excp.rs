// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 exception and IRQ dispatching.

use x86::{controlregs::cr2, irq::*};
use x86_64::structures::idt::PageFaultErrorCode;

use super::{ExceptionContext, gdt, nmi, userspace};
use crate::excp::PageFaultFlags;

core::arch::global_asm!(
    include_str!("excp.S"),
    include_str!("nmi_entry.S"),
    unittest = const cfg!(unittest) as usize,
    trapframe_size = const core::mem::size_of::<ExceptionContext>(),
    kernel_rsp_to_trapframe_offset = const userspace::KERNEL_RSP_TO_TRAPFRAME_OFFSET,
    KDATA = const gdt::KDATA.0,
    KCODE64 = const gdt::KCODE64.0,
    rcx_offset = const core::mem::offset_of!(ExceptionContext, rcx),
    r11_offset = const core::mem::offset_of!(ExceptionContext, r11),
    rdx_offset = const core::mem::offset_of!(ExceptionContext, rdx),
    UDATA = const gdt::UDATA.0,
    UCODE64 = const gdt::UCODE64.0,
    SYSCALL_VECTOR = const LEGACY_SYSCALL_VECTOR,
    nmi_vector = const nmi::VECTOR,
    nmi_tss_ist_offset = const core::mem::offset_of!(x86_64::structures::tss::TaskStateSegment, interrupt_stack_table),
    vector_offset = const core::mem::offset_of!(ExceptionContext, vector),
    rip_offset = const core::mem::offset_of!(ExceptionContext, rip),
    rsp_offset = const core::mem::offset_of!(ExceptionContext, rsp),
);

const _: () = assert!(core::mem::size_of::<ExceptionContext>() == 23 * 8);

pub(super) const LEGACY_SYSCALL_VECTOR: u8 = 0x80;
pub(super) const IRQ_VECTOR_START: u8 = 0x20;
pub(super) const IRQ_VECTOR_END: u8 = 0xff;

/// Dispatches a page fault and panics if unhandled.
fn handle_page_fault(tf: &mut ExceptionContext) {
    let access_flags = err_code_to_flags(tf.error_code)
        .unwrap_or_else(|e| panic!("Invalid #PF error code: {:#x}", e));
    let vaddr = va!(unsafe {
        // SAFETY: Reading CR2 is a privileged but side-effect-free register read.
        cr2()
    });
    if handle_trap!(PAGE_FAULT, vaddr, access_flags) {
        return;
    }
    if tf.fixup_exception() {
        return;
    }
    core::hint::cold_path();
    panic!(
        "Unhandled #PF @ {:#x}, fault_vaddr={:#x}, error_code={:#x} ({:?}):\n{:#x?}\n{}",
        tf.rip,
        vaddr,
        tf.error_code,
        access_flags,
        tf,
        tf.backtrace()
    );
}

// SAFETY: x86 trap assembly resolves this unique Rust ABI symbol and passes a live
// ExceptionContext reference after saving the trap frame.
/// Architecture-specific trap entry point.
#[unsafe(no_mangle)]
fn x86_trap_handler(tf: &mut ExceptionContext) {
    if tf.vector < u64::from(IRQ_VECTOR_START) && nmi::is_active() {
        // NMI can recover exception-table accesses and return from #BP, but
        // must never enter a page-fault path that allocates or schedules.
        let can_fixup = tf.vector == u64::from(PAGE_FAULT_VECTOR)
            || tf.vector == u64::from(GENERAL_PROTECTION_FAULT_VECTOR);
        if tf.vector == 3 || can_fixup && tf.fixup_exception() {
            return;
        }
        karch::stop_cpu();
    }
    let _tf_guard = crate::ExceptionContextGuard::new(tf);
    match tf.vector as u8 {
        PAGE_FAULT_VECTOR => handle_page_fault(tf),
        BREAKPOINT_VECTOR => debug!("#BP @ {:#x} ", tf.rip),
        GENERAL_PROTECTION_FAULT_VECTOR => {
            panic!(
                "#GP @ {:#x}, error_code={:#x}:\n{:#x?}\n{}",
                tf.rip,
                tf.error_code,
                tf,
                tf.backtrace()
            );
        }
        IRQ_VECTOR_START..=IRQ_VECTOR_END => {
            // SAFETY: the interrupt gate cleared IF; tf retains this hardware
            // vector and saved state until the adapter and scheduling tail return.
            unsafe { handle_trap!(IRQ, tf.vector as _) };
        }
        _ => {
            panic!(
                "Unhandled exception {} ({}, error_code={:#x}) @ {:#x}:\n{:#x?}\n{}",
                tf.vector,
                vec_to_str(tf.vector),
                tf.error_code,
                tf.rip,
                tf,
                tf.backtrace()
            );
        }
    }
}

/// Dispatches the architectural NMI with its interrupted context published.
// SAFETY: the dedicated assembly entry calls this symbol with a complete
// ExceptionContext on a live CPU/task stack and the trusted GS base installed.
#[unsafe(no_mangle)]
extern "C" fn x86_nmi_handler(tf: &ExceptionContext) {
    if !nmi::begin_dispatch() {
        return;
    }
    loop {
        {
            let _tf_guard = crate::ExceptionContextGuard::new(tf);
            if let Some(handler) = crate::excp::NMI.first() {
                // SAFETY: entry installed kernel GS and excludes ordinary
                // IRQs/migration. The frame remains live on the local stack.
                unsafe { handler(usize::from(nmi::VECTOR)) };
            }
        }
        if nmi::finish_dispatch() {
            break;
        }
    }
}

/// Returns a mnemonic string for an exception vector.
fn vec_to_str(vec: u64) -> &'static str {
    if vec < 32 {
        EXCEPTIONS[vec as usize].mnemonic
    } else {
        "Unknown"
    }
}

/// Converts a page-fault error code to access flags.
pub(super) fn err_code_to_flags(err_code: u64) -> Result<PageFaultFlags, u64> {
    let code = PageFaultErrorCode::from_bits_truncate(err_code);
    let reserved_bits = (PageFaultErrorCode::CAUSED_BY_WRITE
        | PageFaultErrorCode::USER_MODE
        | PageFaultErrorCode::INSTRUCTION_FETCH
        | PageFaultErrorCode::PROTECTION_VIOLATION)
        .complement();
    if code.intersects(reserved_bits) {
        Err(err_code)
    } else {
        let mut flags = PageFaultFlags::empty();
        if code.contains(PageFaultErrorCode::CAUSED_BY_WRITE) {
            flags |= PageFaultFlags::WRITE;
        } else {
            flags |= PageFaultFlags::READ;
        }
        if code.contains(PageFaultErrorCode::USER_MODE) {
            flags |= PageFaultFlags::USER;
        }
        if code.contains(PageFaultErrorCode::INSTRUCTION_FETCH) {
            flags |= PageFaultFlags::EXECUTE;
        }
        Ok(flags)
    }
}
