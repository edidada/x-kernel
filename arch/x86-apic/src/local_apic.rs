// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU Local APIC lifecycle and local interrupt operations.
//!
//! The boot CPU installs its handle through [`init_local_primary`], every
//! application CPU through [`init_secondary`]. The handle lives in a per-CPU
//! slot for the kernel lifetime. Startup consumes its exclusive handle before
//! publication; runtime borrows only a shared register handle. Source-specific
//! operations serialize hardware state, and every ICR writer uses non-waiting
//! admission so an NMI never waits for an interrupted sender.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kalloc_types::{AllocFlags, KBox};
use kcpu_id_map::RawCpuId;
use kcpu_slot::cpu_slot;
use kerrno::{KError, KResult};
use kspin::NoPreemptIrqSave;
use memaddr::pa;
use memspace::iomap_device;
use x2apic::lapic::{
    IpiCommand, IpiSendError, IpiTarget, LocalApic, LocalApicBuilder, RuntimeLocalApic, xapic_base,
};
use x86_64::instructions::port::Port;

use crate::{APIC_ERROR_VECTOR, APIC_SPURIOUS_VECTOR, APIC_TIMER_VECTOR};

cpu_slot! {
    static LOCAL_APIC_PTR: core::sync::atomic::AtomicUsize =
        core::sync::atomic::AtomicUsize::new(0);
}

cpu_slot! {
    static IRQ_WORK_PENDING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
}

static IS_X2APIC: AtomicBool = AtomicBool::new(false);
static XAPIC_BASE: AtomicU64 = AtomicU64::new(0);

/// Changes only this prepared CPU's LAPIC timer LVT mask. The source owner must
/// exclude competing timer programming; this does not disarm or drain a timer.
pub fn set_local_timer_enabled(enabled: bool) {
    with_local_apic(|lapic| {
        // SAFETY: caller owns local timer programming; the helper pins this
        // prepared CPU and excludes ordinary timer IRQ reentry.
        unsafe { lapic.set_timer_enabled(enabled) };
    });
}

/// Prepares this CPU's owned timer as one-shot, divide-by-one, then enables it.
/// Requires local timer-source ownership; normal task/IRQ context is accepted.
/// Does not sleep. Panics if the local APIC has not been prepared.
pub fn prepare_local_timer() {
    with_local_apic(|lapic| {
        // SAFETY: the timer driver owns this CPU's timer source; ordinary IRQs
        // and migration are excluded for the whole local setup transaction.
        unsafe { lapic.prepare_timer() };
        // SAFETY: same source ownership; the timer handler is installed.
        unsafe { lapic.set_timer_enabled(true) };
    });
}

/// Programs this CPU's owned timer countdown; zero stops it.
/// Requires timer-source ownership. Runs pinned with ordinary IRQs excluded,
/// without sleeping. Panics if local APIC preparation is missing.
pub fn set_local_timer_initial(initial: u32) {
    with_local_apic(|lapic| {
        // SAFETY: caller owns the local timer and this helper retains exclusion.
        unsafe { lapic.set_timer_initial(initial) };
    });
}

// All runtime access is shared. Register metadata never changes after
// publication, and operations cannot expose or reconstruct an exclusive borrow.
fn with_local_apic<R>(f: impl FnOnce(&RuntimeLocalApic) -> R) -> R {
    let _irq_guard = NoPreemptIrqSave::new();
    let lapic = local_apic_ptr();
    assert!(
        !lapic.is_null(),
        "local APIC is not initialized on this CPU"
    );
    // SAFETY: boot publishes a leaked immutable runtime owner on this CPU.
    // The guard pins lookup and access; NMI reentry only creates shared borrows.
    unsafe { f(&*lapic) }
}

/// Signals end-of-interrupt on the current CPU's Local APIC.
pub fn end_of_interrupt() {
    with_local_apic(|lapic| {
        // SAFETY: the current CPU's LAPIC handle is initialized before IRQ
        // dispatch, and EOI targets only that CPU's local APIC registers.
        unsafe { lapic.end_of_interrupt() };
    });
}

/// Captures ESR and completes the current CPU's in-service APIC error vector.
///
/// The IRQ backend calls this only for `APIC_ERROR_VECTOR`, instead of generic
/// Action dispatch. Do not issue another EOI for the same delivery.
pub fn handle_error_interrupt() {
    let flags = with_local_apic(|lapic| {
        // SAFETY: IRQ dispatch owns this in-service error event; the helper
        // pins the CPU and excludes local IRQ reentry into the installed APIC.
        unsafe { lapic.acknowledge_error() }
    });
    debug!("Local APIC error: {flags:?}");
}

/// Converts an 8-bit APIC id into the destination-id encoding used by
/// this platform: x2APIC uses the value directly, xAPIC shifts it into
/// the destination field of the redirect/IPI formats.
pub fn raw_apic_id(id_u8: u8) -> u32 {
    if IS_X2APIC.load(Ordering::Relaxed) {
        id_u8 as u32
    } else {
        (id_u8 as u32) << 24
    }
}

fn cpu_has_x2apic() -> bool {
    match raw_cpuid::CpuId::new().get_feature_info() {
        Some(finfo) => finfo.has_x2apic(),
        None => false,
    }
}

/// Brings up the boot CPU's Local APIC.
///
/// Masks the legacy PIC, selects x2APIC or xAPIC from CPUID, programs the
/// timer/error/spurious vectors, maps the xAPIC MMIO page when needed, and
/// publishes the CPU-local handle.
/// The LAPIC timer stays stopped and masked until the timer driver activates it.
/// LINT0/LINT1 stay masked; this platform has no LINT routing consumer.
/// Requires boot CPU context with IRQs and migration disabled. The kernel-lifetime
/// handle uses NOWAIT; no reclaim or sleeping allocation is permitted here.
///
/// # Errors
/// Owner allocation returns NoMemory before publishing the per-CPU handle or
/// enabling error delivery. MMIO errors propagate as InvalidInput, Io or NoMemory.
///
/// # Panics
///
/// Panics if this CPU is initialized twice or fixed builder invariants fail.
pub fn init_local_primary() -> KResult<()> {
    info!("Initialize Local APIC...");
    // SAFETY: CPL0 boot owns the master PIC data port; 0xff masks its legacy interrupt inputs.
    unsafe { Port::<u8>::new(0x21).write(0xff) };
    // SAFETY: CPL0 boot also owns the slave PIC data port; mask its inputs before APIC routing.
    unsafe { Port::<u8>::new(0xA1).write(0xff) };
    let mut builder = LocalApicBuilder::new();
    builder
        .timer_vector(APIC_TIMER_VECTOR as _)
        .error_vector(APIC_ERROR_VECTOR as _)
        .spurious_vector(APIC_SPURIOUS_VECTOR as _);
    if cpu_has_x2apic() {
        info!("Using x2APIC.");
        IS_X2APIC.store(true, Ordering::Relaxed);
    } else {
        info!("Using xAPIC.");
        // SAFETY: on the xAPIC path, `xapic_base` reads the architectural LAPIC
        // base address, which is then mapped before the builder uses it.
        let base_vaddr = iomap_device(pa!(unsafe { xapic_base() } as usize), 0x1000, "lapic")?;
        XAPIC_BASE.store(base_vaddr.as_usize() as u64, Ordering::Relaxed);
        builder.set_xapic_base(base_vaddr.as_usize() as u64);
    }
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: all required timer, error and spurious \
                  vectors are supplied, and the xAPIC path supplies the mapped base"
    )]
    let mut lapic = builder.build().unwrap();
    // SAFETY: Boot excludes IRQs and migration, this handle exclusively owns
    // the current CPU's hardware-enabled LAPIC, and its xAPIC mapping (if any)
    // is valid. No NMI handler accesses the handle during preparation.
    unsafe { lapic.enable_masked() };
    #[cfg(unittest)]
    tests_preparation::verify_masked_preparation();
    install_local_apic(lapic)?;
    enable_error_delivery();
    Ok(())
}

/// Brings up the Local APIC on an application CPU (spurious/error/timer
/// vectors only; the IO-APIC is owned by the boot CPU).
/// Leaves the LAPIC timer stopped and timer/LINT sources masked, as on the BSP.
/// Requires AP boot context with IRQs and migration disabled; owner allocation
/// uses NOWAIT, not sleeping reclaim.
///
/// # Errors
/// NoMemory leaves the handle unpublished and error delivery masked. Hardware
/// preparation has already happened; the platform must fail this CPU's boot.
///
/// # Panics
/// Panics on missing BSP preparation or duplicate per-CPU initialization.
pub fn init_secondary() -> KResult<()> {
    let mut lapic = build_local_apic();
    // SAFETY: AP bring-up excludes IRQs and migration and uses the supported
    // APIC mode/mapping established by the BSP. This CPU exclusively owns its
    // hardware-enabled LAPIC; no NMI handler accesses this unpublished handle.
    unsafe { lapic.enable_masked() };
    #[cfg(unittest)]
    tests_preparation::verify_masked_preparation();
    install_local_apic(lapic)?;
    enable_error_delivery();
    Ok(())
}

fn enable_error_delivery() {
    // The fixed error handler is statically installed in irq-driver, and the
    // per-CPU handle is now published. This unmasks LVTERR, but BSP/AP startup
    // keeps EFLAGS.IF clear until runtime enables local IRQs. The IDT alone
    // does not allow delivery; timer/LINT retain independent activation policy.
    with_local_apic(|lapic| {
        // SAFETY: Masked preparation completed; this borrow pins/excludes
        // local IRQs and the fixed error handler can now find its APIC handle.
        unsafe { lapic.enable_error() };
    });
}

unittest::path_mod_test!(tests_preparation, "../ut-tests/preparation.rs");

// Normal callers retain CPU/IRQ exclusion and have a bounded hardware-delivery
// retry budget. NMI senders use try_send_nmi_* instead and never enter this loop.
const MAX_ICR_SEND_POLLS: usize = 1_000_000;

#[expect(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: Mandatory IPI submission has no error \
              channel; losing scheduler/TLB/startup notifications would violate caller invariants"
)]
fn send_command(command: IpiCommand, target: IpiTarget) {
    with_local_apic(|lapic| {
        for _ in 0..MAX_ICR_SEND_POLLS {
            // SAFETY: normal callers supply validated vectors/startup targets;
            // the helper retains this prepared CPU and ordinary IRQ exclusion.
            match unsafe { lapic.try_send_ipi(command, target) } {
                Ok(()) => {
                    finish_command(lapic);
                    return;
                }
                Err(IpiSendError::Busy) => core::hint::spin_loop(),
                Err(IpiSendError::InvalidDestination) => break,
            }
        }
        // Mandatory scheduler/TLB notifications cannot silently disappear.
        panic!("local APIC rejected mandatory command {command:?} to {target:?}");
    });
}

// All ordinary ICR senders pass this boundary before returning. Waiting for
// hardware admission completion here closes the case where a later NMI sees
// Busy after the ordinary sender's stack has already returned.
fn finish_command(lapic: &RuntimeLocalApic) {
    for _ in 0..MAX_ICR_SEND_POLLS {
        // SAFETY: the ordinary send scope retains the initialized pinned CPU.
        if !unsafe { lapic.delivery_pending() } {
            try_raise_irq_work(lapic);
            // SAFETY: after flushing, prevent another NMI from observing an
            // in-progress hardware command without a resumable sender stack.
            if !unsafe { lapic.delivery_pending() } {
                return;
            }
        }
        core::hint::spin_loop();
    }
    stop_on_delivery_failure();
}

fn try_raise_irq_work(lapic: &RuntimeLocalApic) {
    let pending = IRQ_WORK_PENDING.current_shared();
    if !pending.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: initialization installed this local fixed vector's handler;
    // caller retains CPU/IRQ exclusion, including the NMI producer path.
    if unsafe {
        lapic.try_send_ipi(
            IpiCommand::Fixed(crate::APIC_IRQ_WORK_VECTOR),
            IpiTarget::ThisCpu,
        )
    }
    .is_ok()
    {
        // IRQs cannot run before this scope returns. Work published by an NMI
        // before this clear is covered by the already submitted ordinary IRQ.
        pending.store(false, Ordering::Release);
    }
}

/// Requests the prepared local ordinary irq_work vector, including from NMI.
/// Requires this CPU's irq_work handler to be installed and CPU migration/IRQs
/// excluded. Does not allocate, log or wait for an interrupted software writer.
/// That writer flushes the request before returning. With no software owner,
/// bounded polling permits autonomous hardware delivery to finish; a stuck
/// controller terminally stops this CPU without entering normal logging.
pub fn raise_local_irq_work() {
    IRQ_WORK_PENDING
        .current_shared()
        .store(true, Ordering::Release);
    let pointer = local_apic_ptr();
    if pointer.is_null() {
        stop_on_delivery_failure();
    }
    // SAFETY: irq_work initialization establishes permanent local publication;
    // callers retain CPU/IRQ exclusion and only borrow shared register metadata.
    let lapic = unsafe { &*pointer };
    if lapic.has_icr_writer() {
        return;
    }
    for _ in 0..MAX_ICR_SEND_POLLS {
        try_raise_irq_work(lapic);
        if !IRQ_WORK_PENDING.current_shared().load(Ordering::Acquire) {
            return;
        }
        // No software owner exists on this IRQ-excluded CPU. Hardware delivery
        // progresses independently, unlike the interrupted-writer case above.
        core::hint::spin_loop();
    }
    stop_on_delivery_failure();
}

fn stop_on_delivery_failure() -> ! {
    x86_64::instructions::interrupts::disable();
    loop {
        x86_64::instructions::hlt();
    }
}

fn raw_ipi_target(target: RawCpuId) -> IpiTarget {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Published x86 CPU identities are hardware \
                  APIC ids, bounded to 32 bits; mandatory IPI calls have no error channel"
    )]
    let id = u32::try_from(target.as_usize()).expect("raw APIC id exceeds 32 bits");
    IpiTarget::Destination(id)
}

/// Sends a fixed IPI to this prepared CPU in normal task/IRQ context.
/// Retains CPU/IRQ exclusion and does not sleep. Panics on failed submission.
pub fn send_ipi_self(interrupt_id: usize) {
    send_command(IpiCommand::Fixed(interrupt_id as u8), IpiTarget::ThisCpu);
}

/// Sends a fixed IPI to a raw APIC destination in normal task/IRQ context.
/// Retains CPU/IRQ exclusion and does not sleep. Panics on failed submission.
pub fn send_ipi_raw(interrupt_id: usize, target: RawCpuId) {
    send_command(
        IpiCommand::Fixed(interrupt_id as u8),
        raw_ipi_target(target),
    );
}

/// Broadcasts a fixed IPI to every other CPU from normal task/IRQ context.
/// Retains CPU/IRQ exclusion and does not sleep. Panics on failed submission.
pub fn send_ipi_all_but_self(interrupt_id: usize) {
    send_command(
        IpiCommand::Fixed(interrupt_id as u8),
        IpiTarget::AllExcludingSelf,
    );
}

/// Sends INIT to a validated AP startup destination during serialized bring-up.
/// Does not sleep. Panics on failed local submission.
///
/// # Safety
/// The caller owns serialized AP bring-up and has prepared the target's startup
/// memory/state. This must not reset an already executing CPU.
pub unsafe fn send_init_ipi_raw(target: RawCpuId) {
    send_command(IpiCommand::Init, raw_ipi_target(target));
}

/// Sends SIPI to a validated AP using a prepared, live low-memory startup page.
/// Requires serialized AP bring-up. Does not sleep; panics on failed submission.
///
/// # Safety
/// The target must be awaiting SIPI and `page` must name live, prepared AP
/// startup code whose memory and stack remain valid through CPU initialization.
pub unsafe fn send_startup_ipi_raw(page: u8, target: RawCpuId) {
    send_command(IpiCommand::Startup(page), raw_ipi_target(target));
}

/// Attempts one NMI command without waiting for a software owner or allocating.
/// Accepted hardware submission has bounded completion polling; controller
/// failure stops the CPU without ordinary logging.
///
/// # Safety
/// Call only on a prepared, pinned CPU with ordinary IRQs masked, including
/// NMI context. Every target CPU must have a ready NMI entry and source handler.
///
/// # Errors
/// BadState means this CPU has no published APIC. ResourceBusy means software
/// admission or hardware delivery is busy; no ICR write occurred. InvalidInput
/// rejects a destination wider than the selected mode can encode.
pub unsafe fn try_send_nmi_raw(target: RawCpuId) -> KResult<()> {
    let destination = u32::try_from(target.as_usize()).map_err(|_| KError::InvalidInput)?;
    // SAFETY: the caller supplies the NMI entry, pinning and target contract.
    unsafe { try_send_nmi(IpiTarget::Destination(destination)) }
}

/// Attempts one NMI broadcast with the same bounded hardware-only completion
/// policy as [`try_send_nmi_raw`]. Never waits for an interrupted software owner.
///
/// # Safety
/// Same as [`try_send_nmi_raw`], for every other CPU, including booting CPUs.
///
/// # Errors
/// Returns BadState or ResourceBusy without submitting an ICR command.
pub unsafe fn try_send_nmi_all_but_self() -> KResult<()> {
    // SAFETY: the caller validated the entry contract on every remote CPU.
    unsafe { try_send_nmi(IpiTarget::AllExcludingSelf) }
}

/// Submits a single already-validated NMI destination.
///
/// # Safety
/// Caller retains CPU pinning and IRQ exclusion and establishes ready NMI
/// handlers on every destination, as required by the public NMI helpers.
unsafe fn try_send_nmi(target: IpiTarget) -> KResult<()> {
    let _guard = kspin::NoPreempt::new();
    let pointer = local_apic_ptr();
    if pointer.is_null() {
        return Err(KError::BadState);
    }
    // SAFETY: this pinned CPU published a permanent immutable runtime owner.
    let lapic = unsafe { &*pointer };
    // SAFETY: the public caller supplies IRQ exclusion and target readiness;
    // runtime admission attempts once, so interrupted senders are never awaited.
    unsafe { lapic.try_send_ipi(IpiCommand::Nmi, target) }.map_err(|error| match error {
        IpiSendError::Busy => KError::ResourceBusy,
        IpiSendError::InvalidDestination => KError::InvalidInput,
    })?;
    // Only autonomous hardware progress is awaited, never an interrupted
    // software owner. Flush irq_work submitted by an NMI over this transaction
    // before returning, so no deferred reclamation notification is stranded.
    finish_command(lapic);
    Ok(())
}

/// Sets the exclusively owned local performance LVT to NMI delivery.
///
/// # Safety
/// Call pinned with ordinary IRQs excluded, including NMI context, after LAPIC
/// initialization and validation of LVTPC support. The caller must own LVTPC
/// and its producer and serialize initial setup, quiescence and rearming.
/// This operation does not stop counters. Panics on missing LAPIC preparation.
pub unsafe fn set_local_performance_nmi_enabled(enabled: bool) {
    let _guard = kspin::NoPreempt::new();
    let pointer = local_apic_ptr();
    assert!(
        !pointer.is_null(),
        "local APIC is not initialized on this CPU"
    );
    // SAFETY: the pinned CPU owns the leaked immutable runtime handle.
    let lapic = unsafe { &*pointer };
    // SAFETY: caller supplies source ownership, capability and setup ordering.
    unsafe { lapic.set_performance_nmi_enabled(enabled) };
}

fn build_local_apic() -> LocalApic {
    let mut builder = LocalApicBuilder::new();
    builder
        .timer_vector(APIC_TIMER_VECTOR as _)
        .error_vector(APIC_ERROR_VECTOR as _)
        .spurious_vector(APIC_SPURIOUS_VECTOR as _);
    if !IS_X2APIC.load(Ordering::Relaxed) {
        let xapic_base = XAPIC_BASE.load(Ordering::Relaxed);
        assert!(xapic_base != 0, "xAPIC base is not initialized");
        builder.set_xapic_base(xapic_base);
    }
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: The builder receives valid fixed APIC \
                  vectors and, for xAPIC, an asserted nonzero MMIO base"
    )]
    builder.build().expect("failed to build local APIC")
}

fn install_local_apic(lapic: LocalApic) -> KResult<()> {
    install_local_apic_with(lapic, KBox::new_with_flags)
}

fn install_local_apic_with(
    lapic: LocalApic,
    prepare: impl FnOnce(RuntimeLocalApic, AllocFlags) -> KResult<KBox<RuntimeLocalApic>>,
) -> KResult<()> {
    let owned = prepare(lapic.into_runtime(), AllocFlags::NOWAIT)?;
    let _guard = NoPreemptIrqSave::new();
    let slot = LOCAL_APIC_PTR.current_shared();
    assert_eq!(slot.load(Ordering::Relaxed), 0);
    // Release publishes the fully initialized immutable runtime handle to NMI
    // lookup. An NMI arriving before publication sees no local owner and fails.
    slot.store(KBox::into_raw(owned) as usize, Ordering::Release);
    Ok(())
}

fn local_apic_ptr() -> *const RuntimeLocalApic {
    // Caller retains CPU pinning. Acquire observes initialization before the
    // unique Release publication; only shared runtime borrows follow it.
    LOCAL_APIC_PTR.current_shared().load(Ordering::Acquire) as *const RuntimeLocalApic
}

/// Exercises the NMI GS transition window without running Rust under user GS.
/// Used only by the serialized native entry test, with a prepared local source.
///
/// # Safety
/// The caller must own and prepare a self-NMI source whose handler increments
/// `completed`. No periodic source or competing ICR sender may run. CPU/IRQs
/// must remain pinned/masked, and both GS bases must be canonical.
#[cfg(unittest)]
pub unsafe fn self_nmi_with_swapped_gs_for_test(
    completed: &core::sync::atomic::AtomicUsize,
) -> bool {
    let before = completed.load(Ordering::Acquire);
    let mut budget = 100_000_000usize;
    if IS_X2APIC.load(Ordering::Relaxed) {
        // SAFETY: exclusive self-NMI test source, ready IST and handler, and
        // known x2APIC mode. Assembly restores GS on both timeout and success;
        // no Rust/GS-relative access occurs inside the swapped interval.
        unsafe {
            core::arch::asm!(
                "swapgs", "mfence", "lfence", "wrmsr",
                "2:", "cmp qword ptr [{completed}], {before}", "jne 3f",
                "pause", "dec {budget}", "jnz 2b", "3:", "swapgs",
                in("ecx") 0x830u32, in("eax") 0x44400u32, in("edx") 0u32,
                completed = in(reg) completed.as_ptr(), before = in(reg) before,
                budget = inout(reg) budget, options(nostack),
            )
        };
    } else {
        let command = (XAPIC_BASE.load(Ordering::Relaxed) + 0x300) as usize;
        // SAFETY: same test-only ownership, with the BSP-established xAPIC
        // mapping. Self shorthand ignores destination-high; no other sender
        // can own the ICR, and IRQ exclusion preserves the command transaction.
        unsafe {
            core::arch::asm!(
                "swapgs", "mov dword ptr [{command}], 0x44400",
                "2:", "cmp qword ptr [{completed}], {before}", "jne 3f",
                "pause", "dec {budget}", "jnz 2b", "3:", "swapgs",
                command = in(reg) command,
                completed = in(reg) completed.as_ptr(), before = in(reg) before,
                budget = inout(reg) budget, options(nostack),
            )
        };
    }
    budget != 0
}
