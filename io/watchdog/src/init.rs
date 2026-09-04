// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Watchdog initialization and optional NMI handler setup.

use alloc::format;
#[cfg(feature = "nmi")]
use alloc::sync::Arc;
#[cfg(all(feature = "nmi", unittest))]
use core::sync::atomic::AtomicUsize;
#[cfg(feature = "nmi")]
use core::sync::atomic::{AtomicBool, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use khal::percpu::this_cpu_id;
use ktime_types::TimeSpan;
use log::{debug, warn};

/// Per-CPU timestamp of the last softlockup report.
/// Rate-limits reports to one per threshold period so a genuine lockup
/// doesn't dump on every timer tick.
#[percpu::def_percpu]
static LAST_SOFTLOCKUP_REPORT: Option<ktime_types::MonotonicInstant> = None;

const SOFTLOCKUP_TOUCH_INTERVAL: TimeSpan = TimeSpan::from_secs(4);
const WATCHDOG_TASK_PRIO: isize = -20;

#[cfg(feature = "nmi")]
static NMI_RENDEZVOUS_IPI_REGISTERED: AtomicBool = AtomicBool::new(false);

#[cfg(all(feature = "nmi", unittest))]
static NMI_IPI_TEST_ACTIVE: AtomicBool = AtomicBool::new(false);
#[cfg(all(feature = "nmi", unittest))]
static NMI_IPI_TEST_ARRIVED: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "nmi")]
const _: () = {
    assert!(
        kbuild_config::WATCHDOG_NMI_IPI_IRQ < 16,
        "watchdog NMI IPI must use an SGI in the range 0..15"
    );
    assert!(
        kbuild_config::WATCHDOG_NMI_IPI_IRQ != kbuild_config::IPI_IRQ,
        "watchdog NMI IPI must not share the ordinary IPI SGI"
    );
};

/// Common watchdog initialization for both primary and secondary CPUs.
///
/// It sets up:
/// - soft lockup detection (timer check + per-CPU watchdog task)
fn init_common() {
    // Hardlockup is "this CPU stopped taking timer IRQs", not "the 4s sample
    // callback did not run between two PMU NMIs". Count every local timer IRQ.
    ktask::register_timer_irq_note(|| {
        crate::timer_tick(khal::time::monotonic_time());
    });
    init_softlockup_detection();

    // Register mutex deadlock check (checked from NMI context only).
    #[cfg(feature = "nmi")]
    crate::register_watchdog_task(&crate::watchdog_task::MUTEX_DEADLOCK_CHECK);
    crate::register_watchdog_task(&SYSTEM_WORKQUEUE_CHECK);

    #[cfg(feature = "nmi")]
    init_nmi_watchdog();

    debug!("watchdog init success on cpu {}", this_cpu_id().as_usize());
}

#[cfg(feature = "nmi")]
fn init_nmi_watchdog() {
    // Degraded build: the compiled-in NMI mechanism is not supported on this
    // hardware, so there is nothing to arm.  `enable_periodic_nmi` would
    // fail the same way; report it once instead of attempting.
    if khal::nmi::mode() == khal::nmi::NmiMode::None {
        log::warn!("[watchdog] NMI mechanism unavailable; hard lockup detection disabled");
        return;
    }

    if NMI_RENDEZVOUS_IPI_REGISTERED.load(Ordering::Acquire) {
        let desc = nmi_rendezvous_irq_desc();
        if !khal::irq::configure_nmi(kbuild_config::WATCHDOG_NMI_IPI_IRQ) {
            log::error!(
                "[watchdog] failed to configure rendezvous SGI {} as NMI on cpu {}; bounded \
                 periodic-NMI fallback remains active",
                kbuild_config::WATCHDOG_NMI_IPI_IRQ,
                this_cpu_id().as_usize()
            );
        } else if let Err(err) = kirq::try_enable(desc, true) {
            log::error!(
                "[watchdog] failed to enable rendezvous SGI {} on cpu {}: {err:?}; bounded \
                 periodic-NMI fallback remains active",
                kbuild_config::WATCHDOG_NMI_IPI_IRQ,
                this_cpu_id().as_usize()
            );
        }
    }

    // Register hard lockup detection task.
    crate::register_hardlockup_detection_task();

    // Start periodic NMI delivery through the source‑neutral NMI interface.
    // The platform backend (currently the PMU cycle counter) internally:
    //   1. Computes the cycle threshold from period_ns.
    //   2. Initialises the per-CPU counter hardware.
    //   3. Registers our per-CPU callback.
    //   4. Promotes the PMU line to NMI delivery on this CPU (PPI).
    //   5. Starts the counter.
    // Steps 2-4 are fallible; the counter and callback are set up *before*
    // the line is promoted, so any failure is rolled back completely by
    // `deinit_cycle_counter()` and promotion is the last fallible step.
    // The PMU overflow-dispatch handler on the interrupt line is registered
    // independently by the `pmu` feature init (kruntime), so perf overflow
    // delivery works even without the hardlockup watchdog.
    if !khal::nmi::enable_periodic_nmi(
        crate::lockup_detection::DEFAULT_HARDLOCKUP_THRESHOLD.as_nanos() as u64,
        watchdog_periodic_nmi,
    ) {
        log::error!(
            "[watchdog] failed to arm periodic NMI on cpu {}; hard lockup detection disabled",
            this_cpu_id().as_usize()
        );
    }
}

#[cfg(feature = "nmi")]
fn nmi_rendezvous_irq_desc() -> kirq::IrqDesc {
    // SGIs are architecturally edge-triggered. Keeping the full GIC descriptor
    // shape consistent across registration and per-CPU enable avoids falling
    // back to a different logical IRQ identity.
    kirq::gic_edge_irq_desc(kbuild_config::WATCHDOG_NMI_IPI_IRQ)
}

/// Register the dedicated rendezvous SGI once, before secondary CPUs leave
/// their boot barrier and before any watchdog periodic NMI can trigger.
#[cfg(feature = "nmi")]
fn register_nmi_rendezvous_ipi() {
    if khal::nmi::mode() == khal::nmi::NmiMode::None {
        return;
    }

    let handler = Arc::new(|_| {
        #[cfg(unittest)]
        if handle_nmi_ipi_test_probe() {
            return kirq::IrqEvent::HANDLED;
        }
        handle_nmi_rendezvous(None);
        kirq::IrqEvent::HANDLED
    });
    if kirq::register_nmi(nmi_rendezvous_irq_desc(), handler) {
        NMI_RENDEZVOUS_IPI_REGISTERED.store(true, Ordering::Release);
    } else {
        log::error!(
            "[watchdog] failed to register rendezvous SGI {}; bounded periodic-NMI fallback \
             remains active",
            kbuild_config::WATCHDOG_NMI_IPI_IRQ
        );
    }
}

/// Periodic watchdog callback invoked in local NMI context.
#[cfg(feature = "nmi")]
fn watchdog_periodic_nmi() {
    use crate::rendezvous as rv;

    // Every periodic NMI checks whether watchdog tasks on this CPU are
    // healthy. The snapshot-session winner becomes the rendezvous cause.
    let fail_name = crate::watchdog_task::check_watchdog_tasks();
    if fail_name.is_some() {
        if ktask::snapshot::nmi_begin() {
            if rv::try_trigger() {
                // Linux-style active backtrace request: wake peer vCPUs with a
                // dedicated NMI SGI instead of waiting for their local PMU
                // overflow, which may stop while KVM has the vCPU in WFI.
                if !NMI_RENDEZVOUS_IPI_REGISTERED.load(Ordering::Acquire)
                    || !khal::nmi::raise_nmi_all_but_self(kbuild_config::WATCHDOG_NMI_IPI_IRQ)
                {
                    khal::kprint_atomic!(
                        "[watchdog] failed to broadcast rendezvous NMI; using bounded \
                         periodic-NMI fallback\n"
                    );
                }
            } else {
                // A stale rendezvous state cannot own the snapshot session we
                // just acquired; release it rather than blocking later dumps.
                ktask::snapshot::nmi_finish();
            }
        } else {
            khal::kprint_atomic!("[watchdog] snapshot already running, skip NMI dump\n");
        }
    }

    handle_nmi_rendezvous(fail_name);
}

/// Join an active watchdog rendezvous from either the PMU NMI or the dedicated
/// cross-CPU SGI. This path is allocation-free and lock-free.
#[cfg(feature = "nmi")]
fn handle_nmi_rendezvous(fail_name: Option<&'static str>) {
    use crate::rendezvous as rv;

    if !rv::is_triggered() {
        return;
    }

    ktask::snapshot::nmi_collect_local();
    // Publish arrival only after the trap-frame slot is complete. The cause
    // CPU's Acquire load of ARRIVED_BITMAP then also observes the snapshot
    // write that precedes this Release operation.
    rv::publish_arrived();
    let this_cpu = this_cpu_id().as_usize();
    if rv::cause_cpu() == Some(this_cpu) {
        let expected = rv::all_arrived_mask();

        khal::kprint_atomic!(
            "[watchdog] failure detected on cpu {}, failed_task={:?}, requesting global NMI \
             snapshot, expected_mask={:#x}\n",
            this_cpu,
            fail_name,
            expected
        );

        // Match Linux's NMI-backtrace failure policy: wait only for a bounded
        // interval, then continue with the CPUs that did respond.
        let arrived = rv::wait_all_arrived(expected, rv::WAIT_TIMEOUT);
        let missing = expected & !arrived;
        if missing != 0 {
            khal::kprint_atomic!(
                "[watchdog] NMI snapshot rendezvous timed out: arrived_mask={:#x}, \
                 missing_mask={:#x}\n",
                arrived,
                missing
            );
        }

        ktask::snapshot::nmi_dump_all(arrived);
        rv::mark_dump_done();
        // Do not clear the snapshot slots here. A bounded rendezvous permits a
        // target that observed Triggered just before the timeout to finish its
        // per-CPU write after `arrived` was sampled. Keeping the session active
        // until the terminal panic avoids racing that late writer with
        // `nmi_finish()`'s bulk clear.
        panic!("Watchdog task check failed (global dump)");
    } else {
        // The cause CPU has a bounded wait and always publishes DumpDone, so
        // responders are not permanently trapped by a missing peer CPU.
        while !rv::is_dump_done() {
            core::hint::spin_loop();
        }
    }
}

#[cfg(all(feature = "nmi", unittest))]
fn handle_nmi_ipi_test_probe() -> bool {
    if !NMI_IPI_TEST_ACTIVE.load(Ordering::Acquire) {
        return false;
    }

    let cpu = this_cpu_id().as_usize();
    if cpu < usize::BITS as usize {
        NMI_IPI_TEST_ARRIVED.fetch_or(1usize << cpu, Ordering::Release);
    }
    true
}

/// Initialize soft lockup detection.
///
/// A per-CPU watchdog task periodically updates a timestamp, and timer
/// callbacks check whether the timestamp is stale.
pub fn init_softlockup_detection() {
    // Explicit sample period — Linux softlockup_thresh/5 (4s), independent of
    // the dynamic schedule timer.
    ktask::register_timer_callback(
        core::time::Duration::from_nanos(
            crate::lockup_detection::DEFAULT_WATCHDOG_SAMPLE_PERIOD.as_nanos_u64_saturating(),
        ),
        |_| {
            let now = khal::time::monotonic_time();
            crate::timer_tick(now);

            if crate::check_softlockup(now) {
                // SAFETY: timer callbacks run with interrupts disabled and cannot
                // migrate while accessing the current CPU's report timestamp.
                let last = unsafe { *LAST_SOFTLOCKUP_REPORT.current_ref_raw() };
                if last.is_none_or(|last| {
                    now.saturating_duration_since(last)
                        > crate::lockup_detection::DEFAULT_SOFTLOCKUP_THRESHOLD
                }) {
                    log::error!(
                        "[watchdog] softlockup detected on cpu {}",
                        this_cpu_id().as_usize()
                    );
                    ktask::dump_sched_stats();
                    // SAFETY: the same non-migrating timer callback exclusively
                    // updates this CPU's report timestamp.
                    unsafe { *LAST_SOFTLOCKUP_REPORT.current_ref_mut_raw() = Some(now) };
                    ktask::snapshot::dump_cpu_tasks(this_cpu_id());
                }
            }
        },
    );

    let cpu_id = this_cpu_id();
    crate::touch_softlockup(khal::time::monotonic_time());
    start_softlockup_watchdog_task(cpu_id);
}

fn start_softlockup_watchdog_task(cpu_id: LogicalCpuId) {
    let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        softlockup_watchdog_task,
        format!("watchdog/{}", cpu_id.as_usize()),
        kbuild_config::TASK_STACK_SIZE,
    ));
    // Spawn pin: write the mask, then activate onto a CPU in that mask
    // (same as ksoftirqd). `set_task_affinity` migrates a task that already
    // occupies a runqueue.
    task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu_id));
    ktask::activate_task(&task);
    if !ktask::set_task_prio(&task, WATCHDOG_TASK_PRIO) {
        warn!(
            "watchdog: failed to raise softlockup watchdog task priority on cpu {}",
            cpu_id.as_usize()
        );
    }
}

fn softlockup_watchdog_task() {
    loop {
        crate::touch_softlockup(khal::time::monotonic_time());
        ktask::sleep(SOFTLOCKUP_TOUCH_INTERVAL);
    }
}

struct SystemWorkqueueCheck;

static SYSTEM_WORKQUEUE_CHECK: SystemWorkqueueCheck = SystemWorkqueueCheck;

impl crate::watchdog_task::WatchdogTask for SystemWorkqueueCheck {
    fn name(&self) -> &str {
        "SystemWorkqueue"
    }

    fn check(&self) -> bool {
        kwork::raw::system_workqueue_watchdog_check(
            khal::time::monotonic_time(),
            crate::lockup_detection::DEFAULT_SOFTLOCKUP_THRESHOLD,
        )
    }
}

/// Initialize watchdogs on the primary CPU.
pub fn init_primary() {
    #[cfg(feature = "nmi")]
    register_nmi_rendezvous_ipi();
    init_common();
}

/// Initialize watchdogs on a secondary CPU.
pub fn init_secondary() {
    init_common();
}

#[cfg(all(feature = "nmi", unittest))]
mod tests {
    use unittest::{assert, def_test};

    use super::*;

    #[def_test(serial)]
    fn dedicated_nmi_ipi_reaches_peer_cpus() {
        let this_bit = 1usize << this_cpu_id().as_usize();
        let expected = crate::rendezvous::all_arrived_mask() & !this_bit;

        NMI_IPI_TEST_ARRIVED.store(0, Ordering::Release);
        NMI_IPI_TEST_ACTIVE.store(true, Ordering::Release);
        let sent = NMI_RENDEZVOUS_IPI_REGISTERED.load(Ordering::Acquire)
            && khal::nmi::raise_nmi_all_but_self(kbuild_config::WATCHDOG_NMI_IPI_IRQ);

        let start = khal::time::monotonic_time();
        let timeout = TimeSpan::from_secs(2);
        let mut arrived = NMI_IPI_TEST_ARRIVED.load(Ordering::Acquire);
        while sent
            && arrived & expected != expected
            && khal::time::monotonic_time().saturating_duration_since(start) < timeout
        {
            core::hint::spin_loop();
            arrived = NMI_IPI_TEST_ARRIVED.load(Ordering::Acquire);
        }
        NMI_IPI_TEST_ACTIVE.store(false, Ordering::Release);

        assert!(sent);
        assert!(arrived & expected == expected);
    }
}
