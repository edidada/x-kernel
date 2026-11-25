// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

static VERIFIED_CPUS: AtomicUsize = AtomicUsize::new(0);

/// Sends a hardware self-NMI while a distinct alternate GS is installed.
/// Returns whether the handler ran and the alternate GS survived NMI return.
///
/// # Safety
/// The serial fixture must retain CPU/IRQ exclusion, own local ICR access,
/// and install an NMI observer that increments `deliveries` on this CPU.
/// Both GS bases must remain available through the assembly sequence.
pub unsafe fn self_nmi_with_swapped_gs(deliveries: &AtomicUsize) -> bool {
    use x86_64::{
        VirtAddr,
        registers::model_specific::{GsBase, KernelGsBase},
    };

    // SAFETY: the fixture owns the live local APIC and excludes normal IRQs.
    if super::with_local_apic(|lapic| unsafe { lapic.delivery_pending() }) {
        return false;
    }
    let previous_alt = KernelGsBase::read();
    let kernel_gs = GsBase::read();
    let test_alt = VirtAddr::new(0x1000);
    KernelGsBase::write(test_alt);
    let before = deliveries.load(Ordering::Acquire);
    let uses_x2apic = usize::from(super::IS_X2APIC.load(Ordering::Relaxed));
    let xapic_icr = super::XAPIC_BASE.load(Ordering::Relaxed) + 0x300;
    let observed_low: u32;
    let observed_high: u32;
    // SAFETY: self shorthand never accesses ICR2. The selected access is
    // either the live xAPIC ICR-low mapping or the x2APIC ICR MSR. The sole
    // observer and the atomic counter stay live. This interval uses only
    // ordinary addresses and reinstalls kernel GS before Rust resumes.
    unsafe {
        core::arch::asm!(
            "swapgs",
            "test {uses_x2apic}, {uses_x2apic}",
            "jz 2f",
            "mov ecx, 0x830",
            "mov eax, 0x44400",
            "xor edx, edx",
            "mfence",
            "lfence",
            "wrmsr",
            "jmp 3f",
            "2:",
            "mov dword ptr [{xapic_icr}], 0x44400",
            "3:",
            "mov ecx, 10000000",
            "4:",
            "cmp qword ptr [{deliveries}], {before}",
            "jne 5f",
            "pause",
            "dec ecx",
            "jnz 4b",
            "5:",
            "mov ecx, 0xc0000101",
            "rdmsr",
            "swapgs",
            uses_x2apic = in(reg) uses_x2apic,
            xapic_icr = in(reg) xapic_icr,
            deliveries = in(reg) deliveries.as_ptr(),
            before = in(reg) before,
            out("ecx") _,
            out("eax") observed_low,
            out("edx") observed_high,
            options(nostack),
        );
    }
    let observed_gs = u64::from(observed_high) << 32 | u64::from(observed_low);
    let restored_kernel_gs = GsBase::read();
    // Restore the fixture's entry state even when it detected a GS mismatch.
    GsBase::write(kernel_gs);
    KernelGsBase::write(previous_alt);
    observed_gs == test_alt.as_u64()
        && restored_kernel_gs == kernel_gs
        && deliveries.load(Ordering::Acquire) != before
}

/// Sends a hardware self-NMI with IF set and reports whether IF survived return.
/// Returns with IRQs masked, including when delivery was not observed.
///
/// # Safety
/// The serial fixture must pin this initialized CPU with preemption disabled,
/// enter with IRQs masked, own local ICR access and retain the NMI observer.
/// Kernel GS and the current stack must remain valid with ordinary IRQs enabled.
pub unsafe fn self_nmi_with_irqs_enabled(deliveries: &AtomicUsize) -> bool {
    // SAFETY: the fixture owns the local APIC and enters with IRQs masked.
    if super::with_local_apic(|lapic| unsafe { lapic.delivery_pending() }) {
        return false;
    }
    let before = deliveries.load(Ordering::Acquire);
    let uses_x2apic = usize::from(super::IS_X2APIC.load(Ordering::Relaxed));
    let xapic_icr = super::XAPIC_BASE.load(Ordering::Relaxed) + 0x300;
    let entry_flags: u64;
    let return_flags: u64;
    // SAFETY: the fixture pins this CPU and keeps kernel GS installed while
    // normal IRQs can run. Self shorthand only writes ICR-low (or its MSR).
    // The stack and observer stay live; CLI restores masked IRQs before Rust.
    unsafe {
        core::arch::asm!(
            "sti",
            "nop",
            "pushfq",
            "pop {entry_flags}",
            "test {uses_x2apic}, {uses_x2apic}",
            "jz 2f",
            "mov ecx, 0x830",
            "mov eax, 0x44400",
            "xor edx, edx",
            "mfence",
            "lfence",
            "wrmsr",
            "jmp 3f",
            "2:",
            "mov dword ptr [{xapic_icr}], 0x44400",
            "3:",
            "mov ecx, 10000000",
            "4:",
            "cmp qword ptr [{deliveries}], {before}",
            "jne 5f",
            "pause",
            "dec ecx",
            "jnz 4b",
            "5:",
            "pushfq",
            "pop {return_flags}",
            "cli",
            uses_x2apic = in(reg) uses_x2apic,
            xapic_icr = in(reg) xapic_icr,
            deliveries = in(reg) deliveries.as_ptr(),
            before = in(reg) before,
            entry_flags = out(reg) entry_flags,
            return_flags = out(reg) return_flags,
            out("ecx") _,
            out("eax") _,
            out("edx") _,
        );
    }
    let interrupt_flag = x86_64::registers::rflags::RFlags::INTERRUPT_FLAG.bits();
    entry_flags & return_flags & interrupt_flag != 0
        && deliveries.load(Ordering::Acquire) != before
}

/// Submits a self-NMI from an already active NMI probe without taking locks.
///
/// # Safety
/// The serial probe owns this initialized CPU's ICR producer and has opened
/// hardware NMI blocking by returning from its deliberate breakpoint.
pub unsafe fn send_test_self_nmi() {
    if super::IS_X2APIC.load(Ordering::Relaxed) {
        // SAFETY: boot selected this live x2APIC; self shorthand has no target
        // word and the probe exclusively owns submission. Publish before IPI.
        unsafe { core::arch::asm!("mfence", "lfence", options(nostack)) };
        // SAFETY: the probe owns this live ICR and self shorthand has no target.
        unsafe { x86_64::registers::model_specific::Msr::new(0x830).write(0x44400) };
    } else {
        let icr = super::XAPIC_BASE.load(Ordering::Relaxed) + 0x300;
        // SAFETY: boot mapped this CPU's APIC and the probe owns ICR-low.
        unsafe { (icr as *mut u32).write_volatile(0x44400) };
    }
}

fn read_register(msr: u32, offset: usize) -> u32 {
    if super::IS_X2APIC.load(Ordering::Relaxed) {
        // SAFETY: Boot selected x2APIC; callers supply only readable LAPIC MSRs.
        unsafe { x86_64::registers::model_specific::Msr::new(msr).read() as u32 }
    } else {
        let base = super::XAPIC_BASE.load(Ordering::Relaxed) as usize;
        // SAFETY: Boot mapped the LAPIC page before this check. Each offset
        // below is a readable, aligned register within that device mapping.
        unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
    }
}

pub(super) fn verify_masked_preparation() {
    // Runs on each booting CPU before handle publication or timer activation.
    // Boot assertions deliberately abort if a caller regresses to enable().
    for (msr, offset) in [
        (0x832, 0x320),
        (0x835, 0x350),
        (0x836, 0x360),
        (0x837, 0x370),
    ] {
        core::assert_ne!(read_register(msr, offset) & (1 << 16), 0);
    }
    core::assert_eq!(read_register(0x838, 0x380), 0);
    core::assert_eq!(read_register(0x832, 0x320) & (3 << 17), 0);
    core::assert_eq!(
        read_register(0x837, 0x370) & 0xff,
        u32::from(super::APIC_ERROR_VECTOR)
    );
    VERIFIED_CPUS.fetch_add(1, Ordering::Relaxed);
}

#[unittest::def_test]
fn boot_checked_masked_preparation() {
    unittest::assert!(VERIFIED_CPUS.load(Ordering::Relaxed) > 0);
}

#[unittest::def_test]
fn owner_failure_preserves_cpu_slot_and_uses_nowait() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let before = super::LOCAL_APIC_PTR
        .current_shared()
        .load(Ordering::Acquire);
    let result = super::install_local_apic_with(super::build_local_apic(), |_, flags| {
        assert_eq!(flags, kalloc_types::AllocFlags::NOWAIT);
        Err(kerrno::KError::NoMemory)
    });
    assert_eq!(result, Err(kerrno::KError::NoMemory));
    assert_eq!(
        super::LOCAL_APIC_PTR
            .current_shared()
            .load(Ordering::Acquire),
        before
    );
}

#[unittest::def_test]
fn mmio_errors_reach_the_kernel_error_boundary() {
    assert_eq!(
        kerrno::KError::from(memspace::IoMapError::InvalidRange),
        kerrno::KError::InvalidInput
    );
    assert_eq!(
        kerrno::KError::from(memspace::IoMapError::MappingFailed),
        kerrno::KError::Io
    );
    assert_eq!(
        kerrno::KError::from(memspace::IoMapError::NoMemory),
        kerrno::KError::NoMemory
    );
}

#[unittest::def_test]
fn directed_fixed_ipi_rejects_unrepresentable_ids_without_writing_icr() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let uses_x2apic = super::IS_X2APIC.load(Ordering::Relaxed);
    // The hardware busy bit can clear independently of this rejected send.
    let command_before = read_register(0x830, 0x300) & !(1 << 12);
    let destination_before = (!uses_x2apic).then(|| read_register(0x830, 0x310));
    assert_eq!(
        super::send_ipi_raw(0x55, super::RawCpuId::new(usize::MAX)),
        Err(kerrno::KError::InvalidInput)
    );
    if !uses_x2apic {
        assert_eq!(
            super::send_ipi_raw(0x55, super::RawCpuId::new(0x100)),
            Err(kerrno::KError::InvalidInput)
        );
    }
    assert_eq!(read_register(0x830, 0x300) & !(1 << 12), command_before);
    if let Some(destination) = destination_before {
        assert_eq!(read_register(0x830, 0x310), destination);
    }
}

#[unittest::def_test]
fn startup_errors_report_busy_invalid_targets_and_pending_completion() -> unittest::TestResult {
    if super::IS_X2APIC.load(Ordering::Relaxed) {
        return unittest::TestResult::Ignored;
    }
    #[repr(C, align(4096))]
    struct RegisterPage([u32; 1024]);
    let mut page = RegisterPage([0; 1024]);
    page.0[0x300 / 4] = 1 << 12;
    page.0[0x310 / 4] = 0x7700_0000;
    let _guard = kspin::NoPreemptIrqSave::new();
    let mut builder = x2apic::lapic::LocalApicBuilder::new();
    builder
        .timer_vector(0xf0)
        .error_vector(0xf2)
        .spurious_vector(0xf1)
        .set_xapic_base(page.0.as_mut_ptr() as u64);
    let lapic = builder.build().unwrap().into_runtime();
    let target = super::IpiTarget::Destination(0x23);
    assert_eq!(
        super::submit_command(&lapic, super::IpiCommand::Init, target, 2),
        Err(kerrno::KError::ResourceBusy)
    );
    assert_eq!(
        super::submit_command(
            &lapic,
            super::IpiCommand::Startup(8),
            super::IpiTarget::Destination(0x100),
            2
        ),
        Err(kerrno::KError::InvalidInput)
    );
    assert_eq!(
        super::finish_command(&lapic, 2),
        Err(kerrno::KError::TimedOut)
    );
    assert_eq!(page.0[0x300 / 4], 1 << 12);
    assert_eq!(page.0[0x310 / 4], 0x7700_0000);
    page.0[0x300 / 4] = 0;
    assert_eq!(
        super::submit_command(
            &lapic,
            super::IpiCommand::Init,
            super::IpiTarget::AllExcludingSelf,
            2
        ),
        Ok(())
    );
    assert_eq!(super::finish_command(&lapic, 2), Ok(()));
    assert_eq!(page.0[0x300 / 4], 0xc4500);
    assert_eq!(page.0[0x310 / 4], 0x7700_0000);
    unittest::TestResult::Ok
}
