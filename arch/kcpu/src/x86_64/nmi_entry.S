# SPDX-License-Identifier: Apache-2.0
# Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
# See LICENSES for license details.

# The frame/replay protocol follows the design described in Linux's x86
# entry_64.S. This implementation uses X-Kernel's GS and UserContext ABI.
# No callback runs on the landing frame reused by hardware IST delivery.
# Stack addresses relative to IST top H (the kernel-GS metadata address):
# H-40: hardware return; H-48: scratch RDX; H-56: execution marker;
# H-96: working return; H-136: immutable return; H-280: ExceptionContext.
# Only the hardware/scratch landing area is overwritten on every arrival.
.section .text
.Lx86_nmi_entry:
    cld
    push    rdx
    test    byte ptr [rsp + 16], 3
    jnz     .Lnmi_from_user

    # Reentry during reconstruction already has a callback pass ahead of it.
    lea     rdx, [rip + .Lnmi_repeat]
    cmp     qword ptr [rsp + 8], rdx
    jb      .Lnmi_not_copying
    lea     rdx, [rip + .Lnmi_repeat_ready]
    cmp     qword ptr [rsp + 8], rdx
    jb      .Lnmi_nested_return
.Lnmi_not_copying:
    cmp     qword ptr [rsp - 8], 1
    je      .Lnmi_nested

    # The execution marker is cleared immediately before the final IRET.
    lea     rdx, [rip + .Lnmi_kernel_iret]
    cmp     qword ptr [rsp + 8], rdx
    je      .Lnmi_nested
    jmp     .Lnmi_first

.Lnmi_nested:
.if {unittest}
    inc     qword ptr [rsp + 56]         # metadata counter, above IST top
.endif
    # Replace the working return with a loop back to the immutable frame.
    # Only RDX is scratch; the landing frame and its RDX slot stay untouched.
    sub     rsp, 8
    lea     rdx, [rsp - 80]
    push    {KDATA}
    push    rdx
    pushfq
    push    {KCODE64}
    lea     rdx, [rip + .Lnmi_repeat]
    push    rdx
    add     rsp, 48
.Lnmi_nested_return:
    pop     rdx
    iretq

.Lnmi_first:
    mov     rdx, [rsp]
    push    0                           # execution marker, IST top - 56
    sub     rsp, 40                     # working return frame
    .rept 5
    push    qword ptr [rsp + 88]         # immutable outer hardware frame
    .endr

.Lnmi_repeat:
    cld
    mov     qword ptr [rsp + 80], 1
    add     rsp, 80
    .rept 5
    push    qword ptr [rsp - 48]
    .endr
    sub     rsp, 40
.Lnmi_repeat_ready:
    push    0
    push    {nmi_vector}
    push    -1
    PUSH_GENERAL_REGS
    mov     r12, rsp
    mov     ecx, 0xc0000101
    rdmsr
    shl     rdx, 32
    or      rax, rdx
    mov     r13, rax
    mov     rax, [r12 + {trapframe_size} + 96]
    mov     rdx, rax
    shr     rdx, 32
    wrmsr
    mov     r14, cr2
    mov     rdi, r12
    and     rsp, -16
    call    x86_nmi_handler
    mov     rsp, r12
    mov     cr2, r14
    mov     ecx, 0xc0000101
    mov     rax, r13
    mov     rdx, rax
    shr     rdx, 32
    wrmsr
    POP_GENERAL_REGS
    add     rsp, 64                     # synthetic fields + immutable frame
    mov     qword ptr [rsp + 40], 0
.Lnmi_kernel_iret:
    iretq

.Lnmi_from_user:
    # CPL 3 guarantees SWAPGS installs kernel GS. The task's UserContext
    # owns a complete saved user frame and its suspended kernel continuation.
    swapgs
    mov     rdx, rsp
    mov     rsp, gs:[offset __KCPU_SLOT_TSS + 4]
    push    qword ptr [rdx + 40]
    push    qword ptr [rdx + 32]
    push    qword ptr [rdx + 24]
    push    qword ptr [rdx + 16]
    push    qword ptr [rdx + 8]
    push    0
    push    {nmi_vector}
    push    -1
    PUSH_GENERAL_REGS
    mov     r12, rsp
    mov     rax, [rdx]
    mov     [r12 + {rdx_offset}], rax
    mov     rsp, [r12 - {kernel_rsp_to_trapframe_offset}]
    mov     r14, cr2
    mov     rdi, r12
    and     rsp, -16
    call    x86_nmi_handler
    mov     cr2, r14
    mov     rsi, r12
    mov     rsp, [r12 - {kernel_rsp_to_trapframe_offset}]
    sub     rsp, {trapframe_size}
    mov     rdi, rsp
    mov     ecx, {trapframe_size} / 8
    rep movsq
    POP_GENERAL_REGS
    add     rsp, 24
    swapgs
.Lnmi_user_iret:
    iretq
.Lx86_nmi_end:

.Lx86_nmi_halt:
    cli
.Lx86_nmi_halt_loop:
    hlt
    jmp     .Lx86_nmi_halt_loop

.if {unittest}
.global x86_nmi_test_nested_count
.type x86_nmi_test_nested_count, @function
x86_nmi_test_nested_count:
    mov     rax, gs:[offset __KCPU_SLOT_TSS + {nmi_tss_ist_offset}]
    mov     rax, [rax + 8]
    ret
.size x86_nmi_test_nested_count, . - x86_nmi_test_nested_count
.endif

.if {unittest}
# Exercise the exact NMI user-return instruction with a deliberately bad CS,
# using the same saved continuation and task-stack copy as the real entry.
.global x86_nmi_test_bad_iret
.type x86_nmi_test_bad_iret, @function
x86_nmi_test_bad_iret:
    mov     rax, gs:[offset __KCPU_SLOT_TSS + 4]
    push    rax
    lea     rax, [rip + .Lnmi_test_user_done]
    push    rax
    PUSH_KERNEL_CTX
    mov     [rdi], rsp
    add     rdi, {kernel_rsp_to_trapframe_offset}
    lea     rax, [rdi + {trapframe_size}]
    mov     gs:[offset __KCPU_SLOT_TSS + 4], rax
    mov     rsi, rdi
    sub     rsp, {trapframe_size}
    mov     rdi, rsp
    mov     ecx, {trapframe_size} / 8
    rep movsq
    swapgs
    POP_GENERAL_REGS
    add     rsp, 24
    jmp     .Lnmi_user_iret
.size x86_nmi_test_bad_iret, . - x86_nmi_test_bad_iret
.endif

.if {unittest}
.global x86_nmi_test_user_origin
.type x86_nmi_test_user_origin, @function
x86_nmi_test_user_origin:
    mov     rax, gs:[offset __KCPU_SLOT_TSS + 4]
    push    rax
    lea     rax, [rip + .Lnmi_test_user_done]
    push    rax
    PUSH_KERNEL_CTX
    mov     [rdi], rsp
    add     rdi, {kernel_rsp_to_trapframe_offset}
    lea     rsi, [rdi + {trapframe_size}]
    mov     gs:[offset __KCPU_SLOT_TSS + 4], rsi
    lea     rsi, [rdi + {rip_offset}]
    mov     rdi, gs:[offset __KCPU_SLOT_TSS + {nmi_tss_ist_offset}]
    sub     rdi, 40
    mov     ecx, 5
    rep movsq
    lea     rsp, [rsi - {trapframe_size}]
    POP_GENERAL_REGS
    mov     rsp, gs:[offset __KCPU_SLOT_TSS + {nmi_tss_ist_offset}]
    sub     rsp, 40
    swapgs
    jmp     .Lx86_nmi_entry
.size x86_nmi_test_user_origin, . - x86_nmi_test_user_origin
.endif

.if {unittest}
.Lnmi_test_user_done:
    pop     rax
    mov     gs:[offset __KCPU_SLOT_TSS + 4], rax
    ret
.section .rodata
.balign 8
.global x86_nmi_test_windows
.type x86_nmi_test_windows, @object
x86_nmi_test_windows:
    .quad .Lnmi_repeat, .Lnmi_repeat_ready, .Lnmi_kernel_iret, .Lnmi_user_iret
.size x86_nmi_test_windows, . - x86_nmi_test_windows
.endif
