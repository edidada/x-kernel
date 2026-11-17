// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Watchdog initialization and optional NMI handler setup.

use alloc::format;

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use kcpu_slot::cpu_slot_cell_with_template;
use khal::percpu::this_cpu_id;
use kspin::NoPreemptIrqSave;
use ktime_types::TimeSpan;
use log::{debug, warn};

// Per-CPU timestamp of the last softlockup report. It rate-limits reports to
// one per threshold period so a genuine lockup does not dump every timer tick.
kcpu_slot::cpu_slot_cell_with_template! {
    // SAFETY: `None` contains no initialized `MonotonicInstant` payload, so
    // copying the template creates independent empty options for every CPU.
    static LAST_SOFTLOCKUP_REPORT: Option<ktime_types::MonotonicInstant> = unsafe { None };
}

const SOFTLOCKUP_TOUCH_INTERVAL: TimeSpan = TimeSpan::from_secs(4);
const WATCHDOG_TASK_PRIO: isize = -20;

// Reused by the timer IRQ. The Stop thread clears `queued` before returning
// from the touch, so the next sample can claim the same slot.
kcpu_slot::cpu_slot_cell_with_template! {
    // SAFETY: `CpuStopWork::empty` owns no heap memory and no queued callback.
    static SOFTLOCKUP_STOP_WORK: ktask::CpuStopWork = unsafe { ktask::CpuStopWork::empty() };
}

/// Common watchdog initialization for both primary and secondary CPUs.
///
/// It sets up:
/// - soft lockup detection (timer check queued onto the per-CPU stop thread)
fn init_common() {
    // Hardlockup is "this CPU stopped taking timer IRQs", not "the 4s sample
    // callback did not run between two PMU NMIs". Count every local timer IRQ.
    ktask::register_timer_irq_note(|| {
        crate::timer_tick(khal::time::monotonic_time());
        #[cfg(feature = "nmi")]
        crate::rendezvous::participate();
    });
    // The timer service is ready, but this CPU may lock up before its first
    // IRQ. Seed the heartbeat now so that case cannot stay uninitialized forever.
    crate::timer_tick(khal::time::monotonic_time());
    init_softlockup_detection();

    // Register mutex deadlock check (checked from NMI context only).
    #[cfg(feature = "nmi")]
    crate::register_watchdog_task(&crate::watchdog_task::MUTEX_DEADLOCK_CHECK);

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

    // Register hard lockup detection task.
    crate::register_hardlockup_detection_task();
    crate::watchdog_task::publish_watchdog_tasks();

    // Start periodic NMI delivery through the source‑neutral NMI interface.
    // The platform owns the complete local configuration transaction. It
    // excludes delivery before publishing counter/callback storage and makes
    // the handler ready before enabling delivery. Failure may retain disabled
    // storage and a preparation obligation when controller teardown must retry.
    // The source's dispatch handler is registered independently during source
    // initialization, not by the watchdog consumer.
    if !khal::nmi::enable_periodic_nmi(
        crate::lockup_detection::DEFAULT_HARDLOCKUP_THRESHOLD.as_nanos() as u64,
        || {
            if crate::rendezvous::is_active() {
                crate::rendezvous::participate();
            } else if let Some(failed_task) = crate::watchdog_task::check_watchdog_tasks() {
                crate::rendezvous::trigger(failed_task);
            }
        },
    ) {
        log::error!(
            "[watchdog] failed to arm periodic NMI on cpu {}; hard lockup detection disabled",
            this_cpu_id().as_usize()
        );
    } else {
        crate::rendezvous::register_cpu();
    }
}

struct SoftlockupTimer;

// The per-CPU slot retains the timer for the kernel lifetime; the `None`
// template owns no payload, so the certified-initializer form is used.
cpu_slot_cell_with_template! {
    static SOFTLOCKUP_TIMER: Option<ktask::Timer<crate::init::SoftlockupTimer>> =
        unsafe { None };
}

#[cfg(unittest)]
static SAMPLE_COUNTS: [core::sync::atomic::AtomicUsize; kbuild_config::NR_CPUS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; kbuild_config::NR_CPUS];

impl ktask::TimerHandler for SoftlockupTimer {
    fn on_expiration(&self, timer: ktask::TimerControl<'_>, _: ktask::Expiration) {
        let now = khal::time::monotonic_time();
        crate::timer_tick(now);

        queue_softlockup_touch();

        if crate::check_softlockup(now) {
            let guard = NoPreemptIrqSave::new();
            let last = LAST_SOFTLOCKUP_REPORT.read(&guard);
            if last.is_none_or(|last| {
                now.saturating_duration_since(last)
                    > crate::lockup_detection::DEFAULT_SOFTLOCKUP_THRESHOLD
            }) {
                log::error!(
                    "[watchdog] softlockup detected on cpu {}",
                    this_cpu_id().as_usize()
                );
                ktask::dump_sched_stats();
                LAST_SOFTLOCKUP_REPORT.write(&guard, Some(now));
                ktask::snapshot::dump_cpu_tasks(this_cpu_id());
            }
        }
        // Start a full interval after completion even when diagnostics ran late.
        if let Err(error) = timer.rearm(
            khal::time::monotonic_time() + crate::lockup_detection::DEFAULT_WATCHDOG_SAMPLE_PERIOD,
        ) {
            log::error!("[watchdog] softlockup sampling stopped: {error:?}");
        }
        #[cfg(unittest)]
        SAMPLE_COUNTS[this_cpu_id().as_usize()].fetch_add(1, core::sync::atomic::Ordering::Release);
    }
}

/// Initialize soft lockup detection once on the current CPU after scheduler setup.
///
/// A retained timer queues a touch on this CPU's stop thread and checks
/// whether the timestamp is stale. A Stop-class touch that never runs while
/// IRQs still fire is the softlockup signal. The callback rearms on that CPU.
///
/// # Panics
/// Panics if this CPU's timer was already initialized or its first deadline
/// exceeds the timer core's nanosecond range.
pub fn init_softlockup_detection() {
    let timer = ktask::Timer::new(SoftlockupTimer);
    {
        let guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: the guard pins this CPU with local IRQs masked, and only
        // initialization mutates the owner slot; the callback and the NMI
        // handler never access it.
        let slot = unsafe { SOFTLOCKUP_TIMER.get_mut_unchecked(&guard) };
        assert!(slot.is_none(), "softlockup timer already initialized");
        let timer = slot.insert(timer);
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the watchdog sampling \
                      timer cannot start; this CPU initialization entry has no error return or \
                      recovery"
        )]
        // SAFETY: the local scheduler/IRQ service is initialized. The per-CPU
        // slot retains storage forever; the handler preserves IRQ context and
        // only performs the existing IRQ-safe local watchdog check/report.
        unsafe {
            timer.start(
                khal::time::monotonic_time()
                    + crate::lockup_detection::DEFAULT_WATCHDOG_SAMPLE_PERIOD,
            )
        }
        .expect("watchdog sampling deadline must fit the timer core");
    }

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
        #[cfg(feature = "workqueue-stall-check")]
        crate::WorkqueueStallInspectorIf::inspect(
            khal::percpu::this_cpu_id(),
            khal::time::monotonic_time(),
            crate::lockup_detection::DEFAULT_SOFTLOCKUP_THRESHOLD,
        );
        ktask::sleep(SOFTLOCKUP_TOUCH_INTERVAL);
    }
}

fn queue_softlockup_touch() {
    let guard = NoPreemptIrqSave::new();
    // SAFETY: the guard pins this CPU. The timer callback and the stop thread
    // on this CPU share the slot; `queued` keeps a second sample from reusing
    // it before the touch returns.
    let work = unsafe { SOFTLOCKUP_STOP_WORK.get_mut_unchecked(&guard) };
    let _ = ktask::stop_one_cpu_nowait(this_cpu_id(), softlockup_fn, core::ptr::null_mut(), work);
}

fn softlockup_fn(_arg: *mut ()) {
    crate::touch_softlockup(khal::time::monotonic_time());
}

/// Initialize watchdogs on the primary CPU.
pub fn init_primary() {
    init_common();
}

/// Initialize watchdogs on a secondary CPU.
pub fn init_secondary() {
    init_common();
}

unittest::path_mod_test!(timer_irq_tests, "../ut-tests/init.rs");
