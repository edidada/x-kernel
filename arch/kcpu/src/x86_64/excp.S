# Parts of this file derived from code from Asterinas:
# https://github.com/asterinas/asterinas/blob/257b0c63b1f039e1ec4fd94c2c7bd549f8db2830/ostd/src/arch/x86/trap/trap.S
# The referenced upstream code is licensed under MPL-2.0.

.code64
.equ NUM_INT, 256

.altmacro
.macro PUSH2, r1, r2
    push    \r1
    push    \r2
.endm

.macro POP2, r1, r2
    pop     \r1
    pop     \r2
.endm

.macro DEF_HANDLER, i
.Ltrap_handler_\i:
.if \i == {nmi_vector}
    jmp     .Lx86_nmi_entry
.elseif \i == 8 || (\i >= 10 && \i <= 14) || \i == 17 || \i == 21 || \i == 29 || \i == 30
    # error code pushed by CPU
    push    \i          # interrupt vector
    jmp     .Ltrap_common
.else
    push    0           # fill in error code in TrapFrame
    push    \i          # interrupt vector
    jmp     .Ltrap_common
.endif
.endm

.macro DEF_TABLE_ENTRY, i
    .quad .Ltrap_handler_\i
.endm

.section .rodata
.global trap_handler_table
trap_handler_table:
.set i, 0
.rept NUM_INT
    DEF_TABLE_ENTRY %i
    .set i, i + 1
.endr

.macro PUSH_GENERAL_REGS
    PUSH2   r15, r14
    PUSH2   r13, r12
    PUSH2   r11, r10
    PUSH2   r9, r8
    PUSH2   rdi, rsi
    PUSH2   rbp, rbx
    PUSH2   rdx, rcx
    push    rax
.endm

.macro POP_GENERAL_REGS
    pop     rax
    POP2    rcx, rdx
    POP2    rbx, rbp
    POP2    rsi, rdi
    POP2    r8, r9
    POP2    r10, r11
    POP2    r12, r13
    POP2    r14, r15
.endm

.macro PUSH_KERNEL_CTX
    PUSH2   rbp, rbx
    PUSH2   r12, r13
    PUSH2   r14, r15
.endm

.macro POP_KERNEL_CTX
    POP2    r15, r14
    POP2    r13, r12
    POP2    rbx, rbp
.endm

# Unexpected faults in NMI glue must not reach Rust with an untrusted GS.
# Identified user IRET faults are adapted separately before this check.
.macro NMI_CHECK_GLUE_RIP
    mov     rcx, [rsp + {rip_offset}]
    lea     rax, [rip + .Lx86_nmi_entry]
    cmp     rcx, rax
    jb      .Lnmi_rip_outside_\@
    lea     rax, [rip + .Lx86_nmi_end]
    cmp     rcx, rax
    jb      .Lx86_nmi_halt
.Lnmi_rip_outside_\@:
.endm

.section .text
.set i, 0
.rept NUM_INT
    DEF_HANDLER %i
    .set i, i + 1
.endr

.Ltrap_common:
    cld
    test    byte ptr [rsp + 3 * 8], 3
    jz      .Ltrap_kernel

.Ltrap_user:
    swapgs                              # swap in kernel gs
    push    -1                          # orig_rax: not a syscall
    jmp     .Lexit_user

.Ltrap_kernel:
    push    -1                          # orig_rax: not a syscall
    PUSH_GENERAL_REGS

    # An exception in NMI glue may arrive with the interrupted GS base
    # installed. Terminate before entering Rust or returning with IRET.
    cmp     qword ptr [rsp + {vector_offset}], 32
    jae     .Ldispatch_kernel_trap
    # A user IRET fault occurs before SWAPGS has been undone. Convert its
    # register frame back into the owning UserContext before calling Rust.
    mov     rax, [rsp + {rip_offset}]
    lea     rcx, [rip + .Lnmi_user_iret]
    cmp     rax, rcx
    je      .Lbad_user_iret
    lea     rcx, [rip + .Liret]
    cmp     rax, rcx
    je      .Lbad_user_iret
    NMI_CHECK_GLUE_RIP
.Ldispatch_kernel_trap:
    mov     rdi, rsp
    call    x86_trap_handler

    POP_GENERAL_REGS
    add     rsp, 24                     # pop orig_rax, vector, error_code
    iretq

# Only #NP/#SS/#GP are return-validation failures. Other faults in this
# trusted return sequence mean kernel corruption and remain terminal.
.Lbad_user_iret:
    mov     rax, [rsp + {vector_offset}]
    sub     rax, 11
    cmp     rax, 2
    ja      .Lx86_nmi_halt
    swapgs
    mov     rdx, gs:[offset __KCPU_SLOT_TSS + 4]
    sub     rdx, {trapframe_size}
    mov     r8, [rsp + {rsp_offset}]
    mov     rdi, rdx
    mov     rsi, rsp
    mov     ecx, {rip_offset} / 8
    rep movsq
    mov     rsi, r8
    mov     ecx, 5
    rep movsq
    mov     rsp, rdx
    jmp     .Lexit_user_restore_kernel

.global syscall_entry
syscall_entry:
    swapgs                              # swap in kernel gs
    mov     gs:[offset __KCPU_SLOT_TSS + 12], rsp # store user rsp -> scratch at TSS.sp1
    mov     rsp, gs:[offset __KCPU_SLOT_TSS + 4]  # load end of TrapFrame <- TSS.sp0
.Lsyscall_kernel_rsp:

    push    {UDATA}                     # push ss
    push    gs:[offset __KCPU_SLOT_TSS + 12]      # push rsp
    push    r11                         # push rflags
    push    {UCODE64}                   # push cs
    push    rcx                         # push rip

    push    0                           # push error_code
    push    {SYSCALL_VECTOR}            # push vector
    push    rax                         # push orig_rax

.Lexit_user:
    PUSH_GENERAL_REGS
.Lexit_user_restore_kernel:

    # Restore kernel context from UserContext.kernel_rsp, stored immediately
    # before the TrapFrame that hardware and this entry code fill.
    mov     rsp, [rsp - {kernel_rsp_to_trapframe_offset}]
    POP_KERNEL_CTX
    ret

.global enter_user
enter_user:
    # save kernel context
    PUSH_KERNEL_CTX
    mov [rdi], rsp

    add rdi, {kernel_rsp_to_trapframe_offset}
    mov rsp, rdi
    add rdi, {trapframe_size}
    mov gs:[offset __KCPU_SLOT_TSS + 4], rdi      # store end of TrapFrame -> TSS.sp0

    # Keep SYSRET on its existing fast path. Only an IRET attempt needs a
    # task-stack copy that protects UserContext.kernel_rsp from return faults.
    mov rax, [rsp + {rcx_offset}]
    cmp rax, [rsp + {rip_offset}]
    jne .Lprepare_user_iret
    mov rax, [rsp + {r11_offset}]
    cmp rax, [rsp + {rip_offset} + 16]
    jne .Lprepare_user_iret
    test rax, 0x10100                   # RF/TF require IRET
    jnz .Lprepare_user_iret
    mov rax, [rsp + {rip_offset}]
    shl rax, 16
    sar rax, 16
    cmp rax, [rsp + {rip_offset}]
    jne .Lprepare_user_iret
    swapgs
    POP_GENERAL_REGS
    add rsp, 24
    mov rsp, [rsp + 24]
.Lsysret_user_rsp:
    sysretq

.Lprepare_user_iret:
    mov rsi, rsp
    mov rsp, [rsi - {kernel_rsp_to_trapframe_offset}]
    sub rsp, {trapframe_size}
    mov rdi, rsp
    mov ecx, {trapframe_size} / 8
    rep movsq
    swapgs
    POP_GENERAL_REGS
    add rsp, 24
.Liret:
    iretq
