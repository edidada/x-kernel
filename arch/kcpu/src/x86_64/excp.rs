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
    trapframe_size = const core::mem::size_of::<ExceptionContext>(),
    kernel_rsp_to_trapframe_offset = const userspace::KERNEL_RSP_TO_TRAPFRAME_OFFSET,
    UDATA = const gdt::UDATA.0,
    UCODE64 = const gdt::UCODE64.0,
    SYSCALL_VECTOR = const LEGACY_SYSCALL_VECTOR,
    nmi_vector = const nmi::VECTOR,
    nmi_stack_bytes = const nmi::STACK_BYTES,
    vector_offset = const core::mem::offset_of!(ExceptionContext, vector),
    rip_offset = const core::mem::offset_of!(ExceptionContext, rip),
    cs_offset = const core::mem::offset_of!(ExceptionContext, cs),
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
        // Returning through an exception would unblock NMI while its IST
        // frame is live. Stop before fault fixups, logging or panic handlers.
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
// ExceptionContext on the local IST stack and the trusted GS base installed.
#[unsafe(no_mangle)]
extern "C" fn x86_nmi_handler(tf: &ExceptionContext) {
    if nmi::is_active() {
        // A nested delivery has reused the IST. The interrupted entry can
        // no longer resume. Leave NMI blocked and avoid all shared locks.
        karch::stop_cpu();
    }
    nmi::set_active(true);
    {
        let _tf_guard = crate::ExceptionContextGuard::new(tf);
        if let Some(handler) = crate::excp::NMI.first() {
            // SAFETY: vector 2 entered the dedicated interrupt gate with IF
            // clear and hardware NMI blocking. GS selects the initialized
            // CPU area. The callback has no scheduling or IRQ-enable tail.
            unsafe { handler(usize::from(nmi::VECTOR)) };
        }
    }
    nmi::set_active(false);
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
