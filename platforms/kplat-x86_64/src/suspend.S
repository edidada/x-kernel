// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// Resume through a firmware real-mode waking vector or an AP startup IPI.
// The platform copies only [s3_wake_start, s3_wake_end) into its reserved page.
.equ s3_gdtr_offset, s3_wake_gdtr - s3_wake_start
.equ s3_cr3_offset, s3_wake_cr3 - s3_wake_start
.equ s3_context_offset, s3_wake_context - s3_wake_start
.equ s3_target_offset, s3_wake_target - s3_wake_start

.section .text
.code16
.balign 16
.global s3_wake_start
s3_wake_start:
    cli
    mov ax, cs
    mov ds, ax
    mov es, ax
    mov ss, ax
    movzx ebx, ax
    shl ebx, 4
    lgdt [s3_gdtr_offset]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    .byte 0x66, 0xea
.global s3_wake_pm_patch
s3_wake_pm_patch:
    .long 0
    .word 8
.code32
.global s3_wake_pm
s3_wake_pm:
    mov ax, 0x18
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov eax, 0x620
    mov cr4, eax
    mov eax, [ebx + s3_cr3_offset]
    mov cr3, eax
    mov ecx, 0xc0000080
    mov eax, 0x901
    xor edx, edx
    wrmsr
    mov eax, 0x80010033
    mov cr0, eax
    .byte 0xea
.global s3_wake_lm_patch
s3_wake_lm_patch:
    .long 0
    .word 0x10
.code64
.global s3_wake_lm
s3_wake_lm:
    lea rbx, [rip + s3_wake_start]
    mov rdi, [rbx + s3_context_offset]
    mov rax, [rbx + s3_target_offset]
    jmp rax
.balign 8
s3_wake_gdtr:
    .word 31
.global s3_wake_gdt_patch
s3_wake_gdt_patch:
    .long 0
.global s3_wake_gdt
s3_wake_gdt:
    .quad 0, 0x00cf9b000000ffff, 0x00af9b000000ffff, 0x00cf93000000ffff
.global s3_wake_cr3
s3_wake_cr3:
    .quad 0
.global s3_wake_context
s3_wake_context:
    .quad 0
.global s3_wake_target
s3_wake_target:
    .quad 0
.global s3_wake_end
s3_wake_end:

.macro S3_SAVE_MSR number, offset
    mov ecx, \number
    rdmsr
    shl rdx, 32
    or rax, rdx
    mov [rdi + \offset], rax
.endm
.macro S3_RESTORE_MSR number, offset
    mov ecx, \number
    mov rax, [rdi + \offset]
    mov rdx, rax
    shr rdx, 32
    wrmsr
.endm

.code64
.global s3_save_and_call
.type s3_save_and_call, @function
// (context, callback, argument): returns once, after callback abort or S3 wake.
s3_save_and_call:
    mov [rdi], rsp
    mov [rdi + 8], rbx
    mov [rdi + 16], rbp
    mov [rdi + 24], r12
    mov [rdi + 32], r13
    mov [rdi + 40], r14
    mov [rdi + 48], r15
    mov r9, rdx
    mov rax, cr3
    mov [rdi + 56], rax
    mov rax, cr0
    mov [rdi + 64], rax
    mov rax, cr4
    mov [rdi + 72], rax
    S3_SAVE_MSR 0xc0000080, 80
    S3_SAVE_MSR 0xc0000101, 88
    S3_SAVE_MSR 0xc0000102, 96
    S3_SAVE_MSR 0xc0000100, 104
    S3_SAVE_MSR 0xc0000081, 112
    S3_SAVE_MSR 0xc0000082, 120
    S3_SAVE_MSR 0xc0000084, 128
    S3_SAVE_MSR 0x1b, 136
    sgdt [rdi + 144]
    sidt [rdi + 160]
    str word ptr [rdi + 176]
    mov rax, cr2
    mov [rdi + 184], rax
    mov rax, cr8
    mov [rdi + 192], rax
    fxsave64 [rdi + 208]
    wbinvd
    mov r12, rdi
    mov rdi, r9
    sub rsp, 8
    call rsi
    add rsp, 8
    mov rdi, r12
    jmp s3_restore_context
.size s3_save_and_call, .-s3_save_and_call

.global s3_restore_context
.type s3_restore_context, @function
s3_restore_context:
    // Both bootstrap and saved roots map this kernel-image text and context.
    mov rax, [rdi + 56]
    mov cr3, rax
    mov rax, [rdi + 72]
    mov cr4, rax
    mov rax, [rdi + 64]
    mov cr0, rax
    S3_RESTORE_MSR 0xc0000080, 80
    lgdt [rdi + 144]
    lidt [rdi + 160]
    mov ax, 0x10
    mov ss, ax
    mov ds, ax
    mov es, ax
    xor eax, eax
    mov fs, ax
    mov gs, ax
    S3_RESTORE_MSR 0xc0000101, 88
    S3_RESTORE_MSR 0xc0000102, 96
    S3_RESTORE_MSR 0xc0000100, 104
    S3_RESTORE_MSR 0xc0000081, 112
    S3_RESTORE_MSR 0xc0000082, 120
    S3_RESTORE_MSR 0xc0000084, 128
    S3_RESTORE_MSR 0x1b, 136
    movzx ecx, word ptr [rdi + 176]
    mov rax, [rdi + 146]
    and byte ptr [rax + rcx + 5], 0xfd
    ltr cx
    mov rax, [rdi + 184]
    mov cr2, rax
    mov rax, [rdi + 192]
    mov cr8, rax
    fxrstor64 [rdi + 208]
    mov rsp, [rdi]
    push 8
    lea rax, [rip + .Ls3_restored_cs]
    push rax
    retfq
.Ls3_restored_cs:
    mov rbx, [rdi + 8]
    mov rbp, [rdi + 16]
    mov r12, [rdi + 24]
    mov r13, [rdi + 32]
    mov r14, [rdi + 40]
    mov r15, [rdi + 48]
    ret
.size s3_restore_context, .-s3_restore_context
