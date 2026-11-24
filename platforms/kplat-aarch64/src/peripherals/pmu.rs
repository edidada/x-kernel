// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use aarch64_pmuv3::pmuv3::PmuCounter;
use kcpu_id_map::{KCpuMask, KCpuMaskExt};
use kiface_cpu_api::{CpuInterruptReadinessIf, CurrentCpuIf};

pub(crate) mod firmware;

/// Boot CPU recorded before any secondary CPU can run PMU registration.
static PMU_BOOT_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
/// One-shot gate for the post-MP SPI activation path.
static PMU_SPI_ACTIVATED: AtomicBool = AtomicBool::new(false);

pub(crate) fn remember_boot_cpu() {
    let cpu = CurrentCpuIf::current_cpu().as_usize();
    let _ = PMU_BOOT_CPU.compare_exchange(usize::MAX, cpu, Ordering::AcqRel, Ordering::Acquire);
}

fn is_boot_cpu() -> bool {
    PMU_BOOT_CPU.load(Ordering::Acquire) == CurrentCpuIf::current_cpu().as_usize()
}
type PerfCb = fn();
const MAX_PMU_COUNTERS: usize = 32;

#[cfg(feature = "nmi-pmu")]
pub(crate) mod lifecycle;

/// Index of the PMU cycle counter (PMCCNTR), used by the periodic NMI backend.
pub const CYCLE_COUNTER_IDX: u32 = (MAX_PMU_COUNTERS - 1) as u32;

pub struct PmuManager {
    counters: [Option<PmuCounter>; MAX_PMU_COUNTERS],
    overflow_handlers: [Option<PerfCb>; MAX_PMU_COUNTERS],
}
// SAFETY: `LazyInit::new()` is an uninitialized marker without a payload or
// owned resource. Each CPU initializes its copied marker independently.
kcpu_slot::cpu_slot_cell_with_template! {
    static PMU: lazyinit::LazyInit<crate::peripherals::pmu::PmuManager> =
        unsafe { lazyinit::LazyInit::new() };
}

/// Prepares this CPU's disabled PMU PPI before activation or counter setup.
/// The source owns its GIC descriptor; the mechanism only supplies the mode.
/// Requires pinned task/IRQ context outside NMI. KIRQ owns admission and the
/// irqchip owns the priority/attribute transaction; no producer is started here.
#[cfg(feature = "nmi-pmu")]
pub(crate) fn prepare_nmi_delivery(irq: usize) -> bool {
    if kirq::context::is_in_nmi() || kplat::nm_irq::mode() == kplat::nm_irq::NmiMode::None {
        return false;
    }
    let _pin = kspin::NoPreemptIrqSave::new();
    match kirq::prepare_percpu_nmi(kirq::gic_level_irq_desc(irq)) {
        Ok(()) => true,
        Err(error) => {
            log::warn!("PMU local NMI preparation failed: irq={irq}, error={error:?}");
            false
        }
    }
}

/// Arms an exclusively owned local periodic source.
///
/// Rejects existing counter ownership and NMI callers before changing hardware
/// or borrowing storage exclusively. A failed rollback retains storage until
/// [`shutdown_periodic_source`] succeeds; callers must retry shutdown before arm.
#[cfg(feature = "nmi-pmu")]
pub(crate) fn arm_periodic_source(period_ns: u64, handler: PerfCb) -> bool {
    if kirq::context::is_in_nmi() {
        return false;
    }
    let guard = kspin::NoPreemptIrqSave::new();
    if !lifecycle::can_start()
        || kplat::nm_irq::mode() == kplat::nm_irq::NmiMode::None
        || PMU
            .get(&guard)
            .get()
            .is_some_and(|pmu| pmu.counters.iter().any(Option::is_some))
    {
        return false;
    }
    // TODO: derive the counter frequency from the platform instead of 2.5 GHz.
    let cycles = (period_ns as u128 * 2_500_000_000u128 / 1_000_000_000u128) as u64;
    let counter = PmuCounter::new_cycle_counter(cycles);
    if counter.check_pmu_support().is_err() {
        return false;
    }
    // Software vacancy alone does not prove the inherited cycle producer is
    // stopped. Quiesce the owned hardware before teardown may clear pending.
    counter.disable();
    let _ = counter.check_and_clear_overflow();
    if !shutdown_periodic_source() {
        return false;
    }
    let irq = of::pmu_irq_or(kbuild_config::PMU_IRQ);
    // Initial teardown may have no preparation record. Preparation independently
    // verifies completed hardware disable and no active claim, even on first use.
    if !prepare_nmi_delivery(irq) {
        let _ = shutdown_periodic_source();
        return false;
    }
    #[cfg(unittest)]
    if tests_shutdown::take_failure(tests_shutdown::AFTER_PREPARATION) {
        let _ = shutdown_periodic_source();
        return false;
    }
    {
        // SAFETY: preparation verified the local PPI is disabled and inactive.
        // No counter is owned; this guard excludes task/ordinary IRQ mutation.
        // The exclusive borrow ends before delivery or the producer is enabled.
        let pmu = unsafe { PMU.get_mut_unchecked(&guard) };
        pmu.call_once(|| PmuManager {
            counters: [const { None }; MAX_PMU_COUNTERS],
            overflow_handlers: [const { None }; MAX_PMU_COUNTERS],
        });
        pmu.counters[CYCLE_COUNTER_IDX as usize] = Some(counter);
        pmu.overflow_handlers[CYCLE_COUNTER_IDX as usize] = Some(handler);
    }
    // Clear stale overflow and publish handler readiness while the PPI remains
    // masked. Otherwise an old pending level could preempt counter activation
    // repeatedly while the handler still observes a disabled software counter.
    #[cfg(unittest)]
    if tests_shutdown::take_failure(tests_shutdown::CLOSE_ADMISSION) {
        lifecycle::set_closed_for_test(true);
    }
    enable(CYCLE_COUNTER_IDX);
    #[cfg(unittest)]
    if tests_shutdown::take_failure(tests_shutdown::BEFORE_DELIVERY) {
        let _ = shutdown_periodic_source();
        return false;
    }
    // A close racing this pinned region either rejects enable or includes this
    // prepared CPU in its stop pass. No exclusive reference survives this point.
    if !is_enabled(CYCLE_COUNTER_IDX)
        || kirq::try_enable_irq(kirq::gic_level_irq_desc(irq)).is_err()
    {
        let _ = shutdown_periodic_source();
        return false;
    }
    true
}

/// Runs a closure with a shared reference to the PMU counter at `index` on the
/// current CPU.
///
/// Counter slots are only changed during per-CPU setup or rollback before a
/// source is enabled; once armed, the [`PmuCounter`] methods used here take
/// `&self`, so they may race safely with PMU IRQ/NMI dispatch.
#[inline]
fn with_counter<F>(index: u32, f: F)
where
    F: FnOnce(&PmuCounter),
{
    let guard = kspin::NoPreemptIrqSave::new();
    let Some(pmu) = PMU.get(&guard).get() else {
        return;
    };
    if let Some(Some(counter)) = pmu.counters.get(index as usize) {
        f(counter);
    }
}

pub fn enable(index: u32) {
    // `PmuCounter::enable` only mutates its atomic enabled state and PMU
    // registers, so shared access remains valid if an NMI observes it.
    with_counter(index, |c| {
        #[cfg(feature = "nmi-pmu")]
        if !lifecycle::can_start() {
            return;
        }
        c.enable();
    });
}
pub fn disable(index: u32) {
    // As `enable`; the terminal NMI quiesce path may race a pending overflow.
    with_counter(index, |c| c.disable());
}
pub fn is_enabled(index: u32) -> bool {
    let mut enabled = false;
    with_counter(index, |counter| enabled = counter.is_enabled());
    enabled
}
pub fn handle_overflows() -> bool {
    // Called from PMU IRQ/NMI context on the current CPU. Active counters
    // expose their state through `&self` plus atomics, so shared access also
    // permits the terminal quiesce path to race with a pending overflow.
    let guard = kspin::NoPreemptIrqSave::new();
    let Some(pmu) = PMU.get(&guard).get() else {
        return false;
    };
    let mut dispatched_any = false;
    for idx in 0..MAX_PMU_COUNTERS {
        let handler = pmu.overflow_handlers[idx];
        let Some(counter) = pmu.counters[idx].as_ref() else {
            continue;
        };
        if counter.handle_overflow().is_ok() {
            dispatched_any = true;
            if let Some(handler) = handler {
                handler();
            }
        }
    }
    dispatched_any
}
/// Stops the exclusive local periodic source without removing the Line owner.
///
/// Implements [`kplat::nm_irq::PeriodicNmiSourceIf::shutdown_periodic_nmi`]:
/// the caller owns shutdown/restart serialization. Rejects NMI, unavailable
/// mechanisms and any allocated event counter. A startup-prepared PPI without
/// a cycle counter still needs disable/teardown. False
/// retains local storage for retry; true permits later local setup, not global
/// handler destruction without completed shutdown on the other participants.
#[cfg(feature = "nmi-pmu")]
pub fn shutdown_periodic_source() -> bool {
    if kirq::context::is_in_nmi() || kplat::nm_irq::mode() == kplat::nm_irq::NmiMode::None {
        return false;
    }
    let guard = kspin::NoPreemptIrqSave::new();
    let pmu = PMU.get(&guard).get();
    // Disabling the PPI would also suppress event-counter consumers. No state
    // changes are permitted unless this source owns the whole local PMU line.
    if pmu.is_some_and(|pmu| {
        pmu.counters[..CYCLE_COUNTER_IDX as usize]
            .iter()
            .any(Option::is_some)
    }) {
        return false;
    }
    let counter = pmu.and_then(|pmu| pmu.counters[CYCLE_COUNTER_IDX as usize].as_ref());
    let has_counter = counter.is_some();
    let desc = kirq::gic_level_irq_desc(of::pmu_irq_or(kbuild_config::PMU_IRQ));
    let stopped = shutdown_local_with(
        || {
            if let Some(counter) = counter {
                counter.disable();
                // NoOverflow also means there is no stale event to clear.
                let _ = counter.check_and_clear_overflow();
            }
        },
        || kirq::try_disable_irq_nosync(desc).is_ok(),
        || {
            #[cfg(unittest)]
            if tests_shutdown::take_failure(tests_shutdown::DURING_TEARDOWN) {
                return false;
            }
            // SAFETY: this CPU is pinned, its sole producer is stopped, and
            // ordinary IRQs stay masked. The GIC teardown verifies disable
            // completion, rejects active claims and clears pending delivery;
            // failure retains storage, so no claim can cross successful reuse.
            unsafe { kirq::teardown_percpu_nmi(desc).is_ok() }
        },
    );
    if !stopped {
        return false;
    }
    if has_counter {
        // SAFETY: the producer and local PPI are stopped, pending/active delivery
        // has been excluded by successful teardown, and no other counters exist.
        // The guard excludes local task/IRQ access; the shared borrow above is over.
        let pmu = unsafe { PMU.get_mut_unchecked(&guard) };
        pmu.counters[CYCLE_COUNTER_IDX as usize] = None;
        pmu.overflow_handlers[CYCLE_COUNTER_IDX as usize] = None;
    }
    true
}

#[cfg(feature = "nmi-pmu")]
fn shutdown_local_with(
    stop: impl FnOnce(),
    disable: impl FnOnce() -> bool,
    teardown: impl FnOnce() -> bool,
) -> bool {
    stop();
    // PerCpu disable is not nested; every retry can safely close the gate again.
    if !disable() {
        return false;
    }
    teardown()
}

#[cfg(feature = "nmi-pmu")]
unittest::path_mod_test!(tests_shutdown, "../../ut-tests/peripherals_pmu_shutdown.rs");
unittest::path_mod_test!(tests_firmware, "../../ut-tests/peripherals_pmu_firmware.rs");
#[cfg(feature = "ipi")]
unittest::path_mod_test!(tests_ordinary, "../../ut-tests/peripherals_pmu_ordinary.rs");
#[macro_export]
macro_rules! pmu_if_impl {
    () => {
        #[kiface::provide]
        impl kplat::perf::PmuInterruptIf {
            fn shutdown_overflow_irq() -> kerrno::KResult<()> {
                #[cfg(feature = "nmi-pmu")]
                {
                    $crate::peripherals::pmu::lifecycle::shutdown()
                }
                #[cfg(not(feature = "nmi-pmu"))]
                {
                    Err(kerrno::KError::Unsupported)
                }
            }

            fn restore_overflow_irq() -> kerrno::KResult<()> {
                #[cfg(feature = "nmi-pmu")]
                {
                    $crate::peripherals::pmu::lifecycle::restore()
                }
                #[cfg(not(feature = "nmi-pmu"))]
                {
                    Err(kerrno::KError::Unsupported)
                }
            }

            fn register_overflow_irq() -> bool {
                let plan = $crate::peripherals::pmu::firmware::parse_pmu_firmware();
                $crate::peripherals::pmu::remember_boot_cpu();
                match plan {
                    $crate::peripherals::pmu::firmware::PmuIrqPlan::Spi(_) => {
                        // SPI routes are established and enabled only after the
                        // all-CPU barrier; no action exists before that point.
                        return true;
                    }
                    $crate::peripherals::pmu::firmware::PmuIrqPlan::Malformed(reason) => {
                        log::warn!("PMU firmware topology is malformed: {reason}");
                        return false;
                    }
                    $crate::peripherals::pmu::firmware::PmuIrqPlan::Unsupported(reason) => {
                        log::warn!("PMU firmware topology is unsupported: {reason}");
                        return false;
                    }
                    $crate::peripherals::pmu::firmware::PmuIrqPlan::Ppi { .. }
                    | $crate::peripherals::pmu::firmware::PmuIrqPlan::Absent => {}
                }
                let irq = of::pmu_irq_or(kbuild_config::PMU_IRQ);
                if matches!(plan, $crate::peripherals::pmu::firmware::PmuIrqPlan::Absent)
                    && irq >= 32
                {
                    log::warn!("PMU SPI fallback has no owner association: irq={irq}");
                    return false;
                }
                let desc = kirq::gic_level_irq_desc(irq);
                // The platform owns this mapping independently of the normal
                // or NMI action. Preparing it does not enable PMU delivery.
                if let Err(err) = kirq::try_map(desc) {
                    log::warn!("PMU mapping preparation failed: desc={desc:?}, error={err:?}");
                    return false;
                }
                let handler = alloc::sync::Arc::new(|_| {
                    $crate::peripherals::pmu::handle_overflows();
                    kirq::IrqEvent::HANDLED
                });
                // Mechanism selection is initialized before PMU registration
                // and remains stable. Unavailable NMI uses an ordinary action,
                // which needs no NMI-local publication or teardown.
                #[cfg(feature = "nmi-pmu")]
                if kplat::nm_irq::NmiMechanismIf::mode() != kplat::nm_irq::NmiMode::None {
                    return kirq::register_nmi_startup(desc, handler);
                }
                if !(16..32).contains(&irq) {
                    log::warn!("PMU IRQ is neither an associated SPI nor a PPI: irq={irq}");
                    return false;
                }
                // Runtime CPU services explicitly activate each local PPI.
                let registered = kirq::try_register_percpu_startup(desc, handler);
                match registered {
                    Ok(true) => true,
                    Ok(false) => {
                        log::warn!("PMU registration rejected: desc={desc:?}");
                        false
                    }
                    Err(err) => {
                        log::warn!("PMU registration failed: desc={desc:?}, error={err:?}");
                        false
                    }
                }
            }

            fn enable_irq() {
                // Firmware parsing and SPI routing use sleepable Task APIs.
                // Keep the IRQ-off guard confined to the local PPI/NMI path.
                let plan = $crate::peripherals::pmu::firmware::parse_pmu_firmware();
                if let $crate::peripherals::pmu::firmware::PmuIrqPlan::Spi(routes) = plan {
                    $crate::peripherals::pmu::try_activate_spi(&routes);
                    return;
                }
                if matches!(
                    plan,
                    $crate::peripherals::pmu::firmware::PmuIrqPlan::Malformed(_)
                        | $crate::peripherals::pmu::firmware::PmuIrqPlan::Unsupported(_)
                ) {
                    return;
                }
                let _pin = kspin::NoPreemptIrqSave::new();
                #[cfg(feature = "nmi-pmu")]
                if !$crate::peripherals::pmu::lifecycle::can_start() {
                    return;
                }
                let irq = of::pmu_irq_or(kbuild_config::PMU_IRQ);
                #[cfg(feature = "nmi-pmu")]
                if kplat::nm_irq::NmiMechanismIf::mode() != kplat::nm_irq::NmiMode::None
                    && !$crate::peripherals::pmu::prepare_nmi_delivery(irq)
                {
                    log::warn!(
                        "PMU IRQ remains disabled after local NMI preparation failure: {irq}"
                    );
                    return;
                }
                // Use the same descriptor as the registration so the enable
                // targets the same resolved line (bare numbers would fall
                // back to the PlainVirq identity path).
                let desc = kirq::gic_level_irq_desc(irq);
                #[cfg(feature = "nmi-pmu")]
                if kirq::try_enable_irq(desc).is_err() {
                    log::warn!("PMU local IRQ enable failed: {irq}");
                }
                #[cfg(not(feature = "nmi-pmu"))]
                kirq::enable(desc, true);
            }
        }
    };
}

/// Establishes all mapped-but-actionless SPI routes before publishing or
/// enabling any handler. Failure withdraws every action already installed in
/// this attempt, so no PMU line can deliver a partial service.
fn activate_spi_routes(routes: &crate::peripherals::pmu::firmware::PmuSpiRoutes) -> bool {
    const MAX_ROUTES: usize = kbuild_config::NR_CPUS;
    let mut virqs: [Option<kirq::Virq>; MAX_ROUTES] = [None; MAX_ROUTES];
    let mut installed = 0usize;
    let Some(options) = spi_register_options() else {
        log::warn!("PMU SPI action name is invalid");
        return false;
    };
    let handler: kirq::Handler = Arc::new(|_| {
        handle_overflows();
        kirq::IrqEvent::HANDLED
    });

    for route in routes.as_slice() {
        let desc = kirq::gic_level_irq_desc(route.hwirq as usize);
        let virq = match kirq::try_map(desc) {
            Ok(virq) => virq,
            Err(err) => {
                log::warn!("PMU SPI mapping failed: desc={desc:?}, error={err:?}");
                rollback_actions(&virqs[..installed]);
                return false;
            }
        };

        let mut mask = KCpuMask::new();
        mask.set_logical(route.cpu, true);
        let snapshot = match kirq::set_irq_affinity(virq, &mask) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                log::warn!(
                    "PMU SPI route failed: hwirq={}, cpu={}, error={err:?}",
                    route.hwirq,
                    route.cpu.as_usize()
                );
                rollback_actions(&virqs[..installed]);
                return false;
            }
        };
        if snapshot.requested != Some(mask)
            || snapshot.effective != Some(mask)
            || snapshot.is_pending
            || snapshot.has_failed_programming
        {
            log::warn!(
                "PMU SPI route is not owner-exact: hwirq={}, cpu={}, snapshot={:?}",
                route.hwirq,
                route.cpu.as_usize(),
                snapshot
            );
            rollback_actions(&virqs[..installed]);
            return false;
        }

        match kirq::try_register_startup(virq, Arc::clone(&handler), options) {
            Ok(true) => {
                virqs[installed] = Some(virq);
                installed += 1;
            }
            Ok(false) => {
                log::warn!("PMU SPI registration rejected: hwirq={}", route.hwirq);
                rollback_actions(&virqs[..installed]);
                return false;
            }
            Err(err) => {
                log::warn!(
                    "PMU SPI registration failed: hwirq={}, error={err:?}",
                    route.hwirq
                );
                rollback_actions(&virqs[..installed]);
                return false;
            }
        }
    }

    for index in 0..installed {
        let Some(virq) = virqs[index] else {
            continue;
        };
        if let Err(err) = kirq::try_enable_irq(virq) {
            log::warn!("PMU SPI enable failed: virq={virq:?}, error={err:?}");
            for virq in virqs[..installed].iter().flatten() {
                let _ = kirq::try_disable_irq_nosync(*virq);
            }
            rollback_actions(&virqs[..installed]);
            return false;
        }
    }
    true
}

fn rollback_actions(virqs: &[Option<kirq::Virq>]) {
    for virq in virqs.iter().flatten() {
        let _ = kirq::try_free_irq(*virq);
    }
}

fn all_spi_owners_ready(routes: &crate::peripherals::pmu::firmware::PmuSpiRoutes) -> bool {
    let ready = CpuInterruptReadinessIf::ready_cpus();
    routes
        .as_slice()
        .iter()
        .all(|route| ready.get_logical(route.cpu))
}

/// Post-MP SPI activation entry. AP hooks and the pre-barrier boot call are
/// no-ops; only the boot CPU after the all-CPU barrier can commit the plan.
pub(crate) fn try_activate_spi(routes: &crate::peripherals::pmu::firmware::PmuSpiRoutes) {
    if !is_boot_cpu() || PMU_SPI_ACTIVATED.load(Ordering::Acquire) {
        return;
    }
    if !all_spi_owners_ready(routes) {
        log::warn!("PMU SPI activation waits for all owner CPUs");
        return;
    }
    if PMU_SPI_ACTIVATED.swap(true, Ordering::AcqRel) {
        return;
    }
    if !activate_spi_routes(routes) {
        PMU_SPI_ACTIVATED.store(false, Ordering::Release);
        log::warn!("PMU SPI activation failed");
    }
}

/// The PMU SPI action is always installed disabled, then enabled only after
/// every route and handler has been committed.
pub(crate) fn spi_register_options() -> Option<kirq::IrqRegisterOptions> {
    let name = kirq::IrqActionName::new("arm-pmu").ok()?;
    Some(
        kirq::IrqRegisterOptions::disabled()
            .restrict_user_affinity()
            .with_name(name),
    )
}

/// Implements the source‑neutral [`kplat::nm_irq::PeriodicNmiSourceIf`] interface
/// using the PMU cycle counter as the NMI source.
///
/// This is the PMU backend of the NMI subsystem: it computes the overflow
/// threshold, promotes the PMU IRQ line to NMI delivery, initialises the
/// cycle counter, and registers the consumer callback.  The line's
/// overflow‑dispatch handler is registered by
/// `khal::pmu::register_overflow_dispatch` (called from the `pmu` feature
/// init) independently of the watchdog — as a normal IRQ handler in pmu-only
/// builds, or as an NMI handler when the PMU is the compiled NMI source
/// (`nmi-pmu`) and the runtime mechanism is available. An unavailable mechanism
/// selects an ordinary action instead, not normal-IRQ dispatch of an NMI action.
/// Counter configuration currently supports
/// the exclusive periodic owner, not general live perf event reconfiguration.
/// Consumers depend on `khal::nmi::enable_periodic_nmi`.
///
/// # Single‑provider note
///
/// `PeriodicNmiSourceIf` allows exactly one provider.  As long as PMU is the only
/// source this macro implements the interface directly; when a second source
/// is added, `peripherals/nmi.rs` must become the sole provider and dispatch
/// to this backend (and the new one) according to the platform strategy.
#[macro_export]
macro_rules! nmi_pmu_if_impl {
    () => {
        use kplat::nm_irq::NmiCb;

        #[kiface::provide]
        impl kplat::nm_irq::PeriodicNmiSourceIf {
            fn enable_periodic_nmi(period_ns: u64, handler: NmiCb) -> bool {
                $crate::peripherals::pmu::arm_periodic_source(period_ns, handler)
            }

            fn quiesce_periodic_nmi() {
                // Keep the counter and callback allocated: this is a
                // one-way terminal path, and a pending PMU interrupt will
                // observe PmuCounter's disabled atomic state and skip the
                // watchdog callback.
                $crate::peripherals::pmu::disable($crate::peripherals::pmu::CYCLE_COUNTER_IDX);
            }

            fn shutdown_periodic_nmi() -> bool {
                $crate::peripherals::pmu::shutdown_periodic_source()
            }
        }
    };
}
