// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};
use unittest::{assert, assert_eq, def_test};
use super::*;

static CALLS: AtomicUsize = AtomicUsize::new(0);
static BAD_CONTEXT: AtomicBool = AtomicBool::new(false);
static WORK_SUBMITTED: AtomicBool = AtomicBool::new(false);
static WORK_COMPLETED: AtomicBool = AtomicBool::new(false);
static NMI_WORK: kirq::irq_work::IrqWork = kirq::irq_work::IrqWork::new(deferred_work);

/// # Safety
/// Static one-shot node remains live forever and is invoked only by the
/// initialized local ordinary IRQ consumer after removing it from its queue.
unsafe fn deferred_work(_: core::ptr::NonNull<kirq::irq_work::IrqWork>) {
    if kirq::context::is_in_nmi() || !kirq::context::is_in_hardirq() || karch::local_irq_enabled() {
        BAD_CONTEXT.store(true, Ordering::Release);
    }
    WORK_COMPLETED.store(true, Ordering::Release);
}

fn sample() {
    if !kirq::context::is_in_nmi() || karch::local_irq_enabled() {
        BAD_CONTEXT.store(true, Ordering::Relaxed);
    }
    if !WORK_SUBMITTED.swap(true, Ordering::AcqRel) {
        // SAFETY: this static node has one admitted NMI submitter, remains
        // pinned forever and its callback preserves ordinary hardirq context.
        if unsafe { kirq::irq_work::queue(core::ptr::NonNull::from(&NMI_WORK)) }.is_err() {
            BAD_CONTEXT.store(true, Ordering::Release);
        }
    }
    CALLS.fetch_add(1, Ordering::Release);
}

// Uses the notification source independently of PMU availability, as vector 2
// and GS/IST correctness must be testable on TCG without virtual PMU counters.
#[def_test(serial)]
fn native_nmi_runs_on_ist_with_irqs_masked_and_preserves_gs() -> unittest::TestResult {
    let pin = kspin::NoPreemptIrqSave::new();
    let local = LOCAL.current_shared();
    if local.is_armed.load(Ordering::Acquire) {
        return unittest::TestResult::Ignored;
    }
    let desc = descriptor(Source::Notification);
    assert!(kirq::prepare_percpu_nmi(desc).is_ok());
    local.callback.store(sample as *mut (), Ordering::Release);
    assert!(kirq::try_enable_irq(desc).is_ok());
    let raw = kcpu_id_map::raw_cpu_id(kplat::cpu::id()).unwrap();
    let before = x86_64::registers::model_specific::GsBase::read();
    for _ in 0..8 {
        let count = CALLS.load(Ordering::Acquire);
        local.notification_pending.store(true, Ordering::Release);
        // SAFETY: this initialized CPU owns the prepared notification source;
        // ordinary IRQs are masked and its native NMI entry is installed.
        assert!(unsafe { x86_apic::try_send_nmi_raw(raw) }.is_ok());
        let limit = khal::time::monotonic_time_nanos() + 1_000_000_000;
        while CALLS.load(Ordering::Acquire) == count {
            assert!(khal::time::monotonic_time_nanos() < limit);
            core::hint::spin_loop();
        }
        assert_eq!(x86_64::registers::model_specific::GsBase::read(), before);
    }
    local.notification_pending.store(true, Ordering::Release);
    // SAFETY: this serial test owns the sole self-NMI source, with no armed
    // PMU/peer producer. Both GS bases originate from valid boot/task setup.
    assert!(unsafe { x86_apic::self_nmi_with_swapped_gs_for_test(&CALLS) });
    assert_eq!(x86_64::registers::model_specific::GsBase::read(), before);
    assert!(!BAD_CONTEXT.load(Ordering::Relaxed));
    assert!(kirq::try_disable_irq_nosync(desc).is_ok());
    // SAFETY: no outstanding self delivery remains; source disabled, pinned.
    assert!(unsafe { kirq::teardown_percpu_nmi(desc) }.is_ok());
    local.callback.store(core::ptr::null_mut(), Ordering::Release);
    assert!(!WORK_COMPLETED.load(Ordering::Acquire));
    drop(pin);
    let limit = khal::time::monotonic_time_nanos() + 1_000_000_000;
    while !WORK_COMPLETED.load(Ordering::Acquire) {
        assert!(khal::time::monotonic_time_nanos() < limit);
        core::hint::spin_loop();
    }
    assert!(!BAD_CONTEXT.load(Ordering::Acquire));
    unittest::TestResult::Ok
}

static PERIODIC_CALLS: [AtomicUsize; kbuild_config::NR_CPUS] =
    [const { AtomicUsize::new(0) }; kbuild_config::NR_CPUS];

fn periodic_sample() {
    if !kirq::context::is_in_nmi() || karch::local_irq_enabled() {
        BAD_CONTEXT.store(true, Ordering::Relaxed);
    }
    PERIODIC_CALLS[kplat::cpu::id().as_usize()].fetch_add(1, Ordering::Release);
}

fn on_cpu(cpu: usize, action: impl FnOnce() -> bool + Send + 'static) -> bool {
    let result = Arc::new(AtomicBool::new(false));
    let completed = result.clone();
    kipi::run_on_cpu_sync(kcpu_id_map::LogicalCpuId::new(cpu), move || {
        completed.store(action(), Ordering::Release);
    }).unwrap();
    result.load(Ordering::Acquire)
}

#[def_test(serial)]
fn periodic_sources_shutdown_and_restart_on_every_cpu() -> unittest::TestResult {
    let cpus = if cfg!(feature = "smp") { kcpu_id_map::nr_cpus() } else { 1 };
    for cpu in 0..cpus {
        if !on_cpu(cpu, || !LOCAL.current_shared().is_armed.load(Ordering::Acquire)) {
            unittest::ktest_println!(
                "x86 periodic source lifecycle test skipped: source already armed on cpu {cpu}"
            );
            return unittest::TestResult::Ignored;
        }
    }
    #[cfg(not(feature = "nmi-peer-timer"))]
    if !on_cpu(0, || LOCAL.current_shared().hardware().is_some()) {
        unittest::ktest_println!(
            "x86 periodic source lifecycle test skipped: no usable PMU counter detected"
        );
        return unittest::TestResult::Ignored;
    }
    for round in 0..2 {
        for (cpu, calls) in PERIODIC_CALLS.iter().enumerate().take(cpus) {
            calls.store(0, Ordering::Release);
            assert!(on_cpu(cpu, || kplat::nm_irq::enable_periodic_nmi(100_000_000, periodic_sample)));
        }
        let deadline = khal::time::monotonic_time_nanos() + 5_000_000_000;
        while PERIODIC_CALLS[..cpus].iter().any(|calls| calls.load(Ordering::Acquire) < 2) {
            assert!(khal::time::monotonic_time_nanos() < deadline);
            core::hint::spin_loop();
        }
        assert!(!BAD_CONTEXT.load(Ordering::Acquire));
        if round == 0 {
            let blocker = kirq::begin_nmi_shutdown(descriptor(Source::Notification)).unwrap();
            assert_eq!(kplat::nm_irq::NativeNmiSourceIf::shutdown_sources(), Err(kerrno::KError::ResourceBusy));
            drop(blocker);
        }
        assert_eq!(kplat::nm_irq::NativeNmiSourceIf::shutdown_sources(), Ok(()));
        for cpu in 0..cpus {
            assert!(on_cpu(cpu, || {
                let local = LOCAL.current_shared();
                !local.is_armed.load(Ordering::Acquire) && local.callback.load(Ordering::Acquire).is_null()
            }));
        }
        assert_eq!(kplat::nm_irq::NativeNmiSourceIf::restore_sources(), Ok(()));
        unittest::ktest_println!("x86 NMI periodic shutdown/restart round {round}: {cpus} CPUs");
    }
    unittest::TestResult::Ok
}
