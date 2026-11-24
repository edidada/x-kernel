// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, def_test};

use super::{ExceptionContext, gdt, nmi};

unsafe extern "C" {
    fn x86_nmi_entry_must_stop(frame: &ExceptionContext, ist_top: usize) -> u32;
    static x86_nmi_test_code_windows: [usize; 6];
}

fn code_windows() -> [usize; 6] {
    // SAFETY: excp.S exports exactly these six permanent code addresses in
    // unittest builds. Reading the table does not execute the target stubs.
    unsafe { x86_nmi_test_code_windows }
}

fn must_stop(frame: &ExceptionContext, ist_top: usize) -> bool {
    // SAFETY: this is the same SysV assembly classifier called by vector 2.
    // It only reads this live frame; ist_top and saved addresses are used for
    // comparisons, never dereferenced. It changes neither GS nor IRQ state.
    let result = unsafe { x86_nmi_entry_must_stop(frame, ist_top) };
    core::assert!(result <= 1);
    result != 0
}

#[def_test(serial)]
fn nmi_reentry_screen_rejects_exception_stubs_on_the_ist_stack() {
    let [_, _, _, _, _, exception_stub] = code_windows();
    // Only address comparisons are performed, so no backing stack is needed.
    let ist_top = 2 * nmi::STACK_BYTES;
    let bottom = ist_top - nmi::STACK_BYTES;
    let mut frame = ExceptionContext {
        cs: u64::from(gdt::KCODE64.0),
        rip: exception_stub as u64,
        ..Default::default()
    };
    // These are interrupted RSPs inside a #GP stub after the outer IRETQ
    // failed, including both edges of the occupied NMI stack.
    for rsp in [bottom, bottom + 1, ist_top - 1] {
        frame.rsp = rsp as u64;
        assert!(must_stop(&frame, ist_top), "RSP {rsp:#x} escaped the IST screen");
    }
    for rsp in [bottom - 1, ist_top, ist_top + 1] {
        frame.rsp = rsp as u64;
        assert!(!must_stop(&frame, ist_top), "RSP {rsp:#x} was outside the IST");
    }
}

#[def_test(serial)]
fn nmi_reentry_screen_distinguishes_user_stack_transition_windows() {
    let [_, _, syscall_entry, kernel_rsp_ready, sysret, exception_stub] = code_windows();
    let ist_top = 2 * nmi::STACK_BYTES;
    let mut frame = ExceptionContext {
        cs: u64::from(gdt::KCODE64.0),
        rsp: (ist_top - 8) as u64,
        ..Default::default()
    };
    for rip in [syscall_entry, kernel_rsp_ready - 1, sysret] {
        frame.rip = rip as u64;
        assert!(!must_stop(&frame, ist_top));
    }
    // Once SYSCALL installed the kernel RSP, an IST stack must be rejected.
    for rip in [syscall_entry - 1, kernel_rsp_ready, sysret - 1, sysret + 1, exception_stub] {
        frame.rip = rip as u64;
        assert!(must_stop(&frame, ist_top));
    }
    // Ring-3 controls both saved addresses. Even a value within the kernel
    // stack or glue ranges must not be interpreted as kernel reentry.
    frame.cs = u64::from(gdt::UCODE64.0);
    for rip in [syscall_entry, kernel_rsp_ready, exception_stub, code_windows()[0]] {
        frame.rip = rip as u64;
        assert!(!must_stop(&frame, ist_top));
    }
}

#[def_test(serial)]
fn nmi_reentry_screen_checks_glue_independently_of_stack_bounds() {
    let [glue_begin, glue_end, _, _, _, _] = code_windows();
    let ist_top = 2 * nmi::STACK_BYTES;
    let mut frame = ExceptionContext {
        cs: u64::from(gdt::KCODE64.0),
        rsp: ist_top as u64,
        ..Default::default()
    };
    for rip in [glue_begin, glue_end - 1] {
        frame.rip = rip as u64;
        assert!(must_stop(&frame, ist_top));
    }
    for rip in [glue_begin - 1, glue_end] {
        frame.rip = rip as u64;
        assert!(!must_stop(&frame, ist_top));
    }
}
