// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::ExceptionContext;

#[def_test]
fn nmi_uses_a_present_interrupt_gate_and_dedicated_ist() {
    use x86_64::instructions::tables::sidt;

    const GATE_BYTES: usize = 16;
    const GATE_OPTIONS_OFFSET: usize = 4;
    const GATE_PRESENT_AND_TYPE_MASK: u16 = 0x8f00;
    const PRESENT_INTERRUPT_GATE: u16 = 0x8e00;
    const GATE_IST_MASK: u16 = 0x7;

    let idtr = sidt();
    let entry_offset = usize::from(super::nmi::VECTOR) * GATE_BYTES;
    assert!(entry_offset + GATE_BYTES <= usize::from(idtr.limit) + 1);
    let options = (idtr.base.as_u64() as usize + entry_offset + GATE_OPTIONS_OFFSET) as *const u16;
    // SAFETY: the loaded IDT is permanent and immutable after publication.
    // The IDTR limit check covers both option bytes in this 64-bit gate.
    let options = unsafe { options.read_unaligned() };
    assert_eq!(options & GATE_PRESENT_AND_TYPE_MASK, PRESENT_INTERRUPT_GATE);
    assert_eq!(options & GATE_IST_MASK, super::nmi::IST_INDEX + 1);
}

#[def_test]
fn test_exception_context_args() {
    let mut ctx = ExceptionContext::default();
    ctx.set_arg0(10);
    ctx.set_arg1(20);
    ctx.set_arg2(30);
    assert_eq!(ctx.arg0(), 10);
    assert_eq!(ctx.arg1(), 20);
    assert_eq!(ctx.arg2(), 30);
}

#[def_test]
fn test_exception_context_ip_sp() {
    let mut ctx = ExceptionContext::default();
    ctx.set_ip(0x2000);
    ctx.set_sp(0x3000);
    assert_eq!(ctx.ip(), 0x2000);
    assert_eq!(ctx.sp(), 0x3000);
}

#[def_test(serial)]
fn test_exception_context_user_mode() {
    use x86_64::{PrivilegeLevel, registers::segmentation::SegmentSelector};

    use super::gdt::{KCODE64, UCODE64};

    // The user code selector this kernel loads carries RPL 3.
    let mut ctx = ExceptionContext {
        cs: UCODE64.0 as u64,
        ..Default::default()
    };
    assert!(ctx.user_mode());
    // Classification is RPL-based: any Ring3 selector is user mode.
    ctx.cs = SegmentSelector::new(3, PrivilegeLevel::Ring3).0 as u64;
    assert!(ctx.user_mode());
    // Kernel code selectors carry RPL 0.
    ctx.cs = KCODE64.0 as u64;
    assert!(!ctx.user_mode());
    ctx.cs = SegmentSelector::new(2, PrivilegeLevel::Ring0).0 as u64;
    assert!(!ctx.user_mode());

    // The entry frame UserContext::new builds from the same GDT selectors
    // must classify as user mode.
    let uctx = super::userspace::UserContext::new(0x1000, va!(0x2000), 0);
    assert!(uctx.user_mode());

    // The tick-classification accessor must report these same frames
    // through the active-frame slot.
    ctx.cs = UCODE64.0 as u64;
    {
        let _guard = crate::ExceptionContextGuard::new(&ctx);
        assert_eq!(crate::active_exception_user_mode(), Some(true));
    }
    ctx.cs = KCODE64.0 as u64;
    {
        let _guard = crate::ExceptionContextGuard::new(&ctx);
        assert_eq!(crate::active_exception_user_mode(), Some(false));
    }
    assert_eq!(crate::active_exception_user_mode(), None);
}

#[def_test]
fn test_exception_context_sysno_retval() {
    let mut ctx = ExceptionContext::default();
    ctx.set_sysno(7);
    assert_eq!(ctx.sysno(), 7);
    assert_eq!(ctx.orig_sysno(), 7);
    assert_eq!(ctx.retval(), 7);
    ctx.set_retval(9);
    assert_eq!(ctx.sysno(), 7);
    assert_eq!(ctx.orig_sysno(), 7);
    assert_eq!(ctx.retval(), 9);
}

#[def_test]
fn test_exception_context_rollback_syscall() {
    let mut ctx = ExceptionContext::default();
    ctx.set_sysno(202);
    ctx.set_ip(0x1002);
    ctx.set_retval((-512isize) as usize);
    ctx.rollback_syscall();
    assert_eq!(ctx.retval(), 202);
    assert_eq!(ctx.ip(), 0x1000);
}

#[def_test]
fn test_exec_reset_clears_gprs() {
    use super::userspace::UserContext;

    let mut uctx = UserContext::new(0x1004, va!(0x2000), 7);
    // Simulate a syscall trap carrying execve(path, argv, envp): the
    // third argument leaves a stale heap pointer in rdx.
    uctx.rax = 0xbaad;
    uctx.rcx = 1;
    uctx.rdx = 0x4004_f000;
    uctx.rbx = 2;
    uctx.rbp = 3;
    uctx.rsi = 4;
    uctx.rdi = 5;
    uctx.r8 = 6;
    uctx.r9 = 7;
    uctx.r10 = 8;
    uctx.r11 = 9;
    uctx.r12 = 10;
    uctx.r13 = 11;
    uctx.r14 = 12;
    uctx.r15 = 13;
    uctx.orig_rax = 0x3b; // execve syscall number
    uctx.gs_base = 0x1234;

    uctx.reset_for_exec();

    assert_eq!(uctx.rax, 0);
    assert_eq!(uctx.rcx, 0);
    assert_eq!(uctx.rdx, 0);
    assert_eq!(uctx.rbx, 0);
    assert_eq!(uctx.rbp, 0);
    assert_eq!(uctx.rsi, 0);
    assert_eq!(uctx.rdi, 0);
    assert_eq!(uctx.r8, 0);
    assert_eq!(uctx.r9, 0);
    assert_eq!(uctx.r10, 0);
    assert_eq!(uctx.r11, 0);
    assert_eq!(uctx.r12, 0);
    assert_eq!(uctx.r13, 0);
    assert_eq!(uctx.r14, 0);
    assert_eq!(uctx.r15, 0);
    assert_eq!(uctx.orig_rax, u64::MAX);
    assert_eq!(uctx.gs_base, 0);
    // Control fields owned by the caller are not overwritten.
    assert_eq!(uctx.rip, 0x1004);
    assert_eq!(uctx.rsp, 0x2000);
    assert_eq!(uctx.fs_base, 0);
}

#[def_test]
fn test_exec_reset_entry_can_be_reestablished() {
    use super::userspace::UserContext;

    let mut uctx = UserContext::new(0x1004, va!(0x2000), 7);
    uctx.rdx = 0x4004_f000;
    uctx.reset_for_exec();

    // The execve caller re-establishes the ELF entry state afterwards.
    uctx.set_ip(0x2004);
    uctx.set_sp(0x3000);
    uctx.set_tls(0);

    assert_eq!(uctx.rdx, 0);
    assert_eq!(uctx.rip, 0x2004);
    assert_eq!(uctx.rsp, 0x3000);
}

#[def_test(serial)]
fn nmi_bad_user_iret_restores_kernel_gs_and_reports_user_exception() {
    use x86_64::{
        instructions::interrupts::without_interrupts,
        registers::model_specific::GsBase,
        structures::tss::TaskStateSegment,
    };

    unsafe extern "C" {
        fn x86_nmi_test_bad_iret(context: &mut super::userspace::UserContext);
    }

    let (vector, target_rip, gs_restored) = without_interrupts(|| {
        let original_gs = GsBase::read();
        let tss = original_gs.as_u64() as usize
            + kcpu_slot::__cpu_slot_symbol_offset!(super::gdt::__KCPU_SLOT_TSS);
        let rsp0 = (tss + core::mem::offset_of!(TaskStateSegment, privilege_stack_table)) as *mut u64;
        // SAFETY: IRQ exclusion pins this CPU; its permanent TSS contains
        // this packed RSP0 field. No observer retains a Rust reference.
        let previous_rsp0 = unsafe { rsp0.read_unaligned() };
        let mut context = super::userspace::UserContext::new(0x1234, va!(0x2000), 0);
        context.cs = 0; // Deliberately invalid user-return CS, producing #GP.
        // SAFETY: this test wrapper saves a real kernel continuation, copies
        // the frame to the task stack and jumps to the production NMI IRET.
        // The #GP return adapter must restore kernel GS before returning.
        unsafe { x86_nmi_test_bad_iret(&mut context) };
        // SAFETY: restore the original current-CPU RSP0 before IRQ restore.
        unsafe { rsp0.write_unaligned(previous_rsp0) };
        (context.vector, context.rip, GsBase::read() == original_gs)
    });
    assert_eq!(vector, 13);
    assert_eq!(target_rip, 0x1234);
    assert!(gs_restored);
}

#[def_test(serial)]
fn user_origin_nmi_moves_off_ist_and_preserves_invalid_user_target() {
    use x86_64::{
        instructions::interrupts::without_interrupts,
        registers::model_specific::GsBase,
        structures::tss::TaskStateSegment,
    };

    unsafe extern "C" {
        fn x86_nmi_test_user_origin(context: &mut super::userspace::UserContext);
    }

    let (vector, target_rip, gs_restored) = without_interrupts(|| {
        let original_gs = GsBase::read();
        let tss = original_gs.as_u64() as usize
            + kcpu_slot::__cpu_slot_symbol_offset!(super::gdt::__KCPU_SLOT_TSS);
        let rsp0 = (tss + core::mem::offset_of!(TaskStateSegment, privilege_stack_table)) as *mut u64;
        // SAFETY: IRQ exclusion pins this CPU's permanent packed TSS field.
        let previous_rsp0 = unsafe { rsp0.read_unaligned() };
        let mut context = super::userspace::UserContext::new(0x4000_0000_0000, va!(0x2000), 0);
        context.rflags = 0; // Keep IRQs masked through the synthetic fixture.
        // SAFETY: the assembly constructs an IST frame with ring-3 CS and
        // installs alternate GS, then enters the production user-NMI path.
        // The invalid target must fault before any user instruction executes.
        unsafe { x86_nmi_test_user_origin(&mut context) };
        // SAFETY: restore the original CPU-local RSP0 before restoring IRQs.
        unsafe { rsp0.write_unaligned(previous_rsp0) };
        (context.vector, context.rip, GsBase::read() == original_gs)
    });
    assert_eq!(vector, 14);
    assert_eq!(target_rip, 0x4000_0000_0000);
    assert!(gs_restored);
}

#[def_test(serial)]
fn ordinary_user_return_reports_an_unmapped_target_through_run() {
    use x86_64::{
        instructions::interrupts::without_interrupts,
        registers::model_specific::{FsBase, GsBase, KernelGsBase},
        structures::tss::TaskStateSegment,
    };

    let (is_user_fault, vector, target_rip) = without_interrupts(|| {
        let tss = GsBase::read().as_u64() as usize
            + kcpu_slot::__cpu_slot_symbol_offset!(super::gdt::__KCPU_SLOT_TSS);
        let rsp0 = (tss + core::mem::offset_of!(TaskStateSegment, privilege_stack_table)) as *mut u64;
        // SAFETY: this CPU is pinned by IRQ exclusion until on_user_exit.
        let previous_rsp0 = unsafe { rsp0.read_unaligned() };
        let mut context = super::userspace::UserContext::new(0x4000_0000_0000, va!(0x2000), 0);
        context.rflags = 0;
        context.fs_base = FsBase::read().as_u64();
        context.gs_base = KernelGsBase::read().as_u64();
        let reason = context.run(
            || {},
            || {
                // SAFETY: run calls this exit callback on the same CPU with
                // IRQs still masked, before it can enable IRQs or migrate.
                unsafe { rsp0.write_unaligned(previous_rsp0) };
            },
        );
        (matches!(reason, crate::userspace_common::ReturnReason::PageFault(..)), context.vector, context.rip)
    });
    assert!(is_user_fault);
    assert_eq!(vector, 14);
    assert_eq!(target_rip, 0x4000_0000_0000);
}
