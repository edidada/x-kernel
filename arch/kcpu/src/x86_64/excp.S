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

# Available both to the ordinary exception path and to vector 2 itself.
# An IRET fault can reopen NMI delivery before normal trap handling begins.
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
    NMI_CHECK_GLUE_RIP
.Ldispatch_kernel_trap:
    mov     rdi, rsp
    call    x86_trap_handler

    POP_GENERAL_REGS
    add     rsp, 24                     # pop orig_rax, vector, error_code
    iretq

.Lx86_nmi_entry:
    cld
    push    0                           # no hardware error code
    push    {nmi_vector}
    push    -1                          # orig_rax: not a syscall
    PUSH_GENERAL_REGS
    # User RIP and RSP are not evidence that kernel NMI glue is active.
    test    byte ptr [rsp + {cs_offset}], 3
    jnz     .Lnmi_install_gs
    NMI_CHECK_GLUE_RIP

    # A faulting IRETQ unblocks NMI before the exception stub runs. Its RIP
    # is outside the NMI glue, but it is still running on this CPU's IST.
    # Check the saved RSP before touching GS or dispatching another callback.
    mov     rcx, [rsp + {rsp_offset}]
    lea     rax, [rsp + {trapframe_size}]
    cmp     rcx, rax
    jae     .Lnmi_install_gs
    sub     rax, {nmi_stack_bytes}
    cmp     rcx, rax
    jb      .Lnmi_install_gs

    # CPL alone does not make RSP trustworthy in the SYSCALL/SYSRET windows.
    mov     rcx, [rsp + {rip_offset}]
    lea     rax, [rip + syscall_entry]
    cmp     rcx, rax
    jb      .Lx86_nmi_halt
    lea     rax, [rip + .Lsyscall_kernel_rsp]
    cmp     rcx, rax
    jb      .Lnmi_install_gs
    lea     rax, [rip + .Lsysret_user_rsp]
    cmp     rcx, rax
    jne     .Lx86_nmi_halt

.Lnmi_install_gs:
    mov     r12, rsp
    # The IST points to aligned metadata in the permanent CPU area. CS
    # alone cannot identify which GS is active in a SWAPGS transition.
    mov     ecx, 0xc0000101              # IA32_GS_BASE
    rdmsr
    shl     rdx, 32
    or      rax, rdx
    mov     r13, rax
    mov     rax, [r12 + {trapframe_size}]
    mov     rdx, rax
    shr     rdx, 32
    wrmsr

    mov     rdi, r12
    and     rsp, -16
    call    x86_nmi_handler
    mov     rsp, r12

    mov     ecx, 0xc0000101
    mov     rax, r13
    mov     rdx, rax
    shr     rdx, 32
    wrmsr
    POP_GENERAL_REGS
    add     rsp, 24
    iretq
.Lx86_nmi_end:

.Lx86_nmi_halt:
    cli
.Lx86_nmi_halt_loop:
    hlt
    jmp     .Lx86_nmi_halt_loop

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

    swapgs                              # swap in user gs

    POP_GENERAL_REGS
    add rsp, 24                         # pop orig_rax, vector, error_code

    # Determine whether to use sysret or iret.
    # If returning to user space with a clean context,
    # the fast sysret path can be used;
    # otherwise, the slower iret path should be used.
    # Reference: <https://elixir.bootlin.com/linux/v6.0.9/source/arch/x86/entry/entry_64.S#L122>.

    cmp qword ptr [rsp], rcx            # sysret requires rcx = rip
    jne .Liret

    cmp qword ptr [rsp + 16], r11       # sysret requires r11 = rflags
    jne .Liret

    test r11, 0x10100                   # sysret requires rflags not contain RF and TF flags
    jnz .Liret

    shl rcx, 16
    sar rcx, 16
    cmp qword ptr [rsp], rcx            # sysret requires rip be a canonical address
    je .Lsysret
    mov rcx, [rsp]

.Liret:
    iretq

.Lsysret:
    mov rsp, [rsp + 24]                 # load user rsp
.Lsysret_user_rsp:
    sysretq
