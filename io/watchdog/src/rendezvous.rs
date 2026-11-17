// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Best-effort fatal reports from pinned IRQ/NMI context with ordinary IRQs
//! masked. No allocation, normal console lock, scheduler wait or reset occurs.
//! Responders retain their interrupted stacks by parking until reset. Only the
//! reporter waits for peers, with both a time limit and a finite poll budget.

use core::sync::atomic::{AtomicUsize, Ordering};

use khal::percpu::this_cpu_id;
use ktime_types::TimeSpan;

mod state;
use state::{Participation, ReportState, WaitBudget, cpu_bit};

// A healthy idle CPU has a watchdog timer deadline every four seconds. Timer
// IRQ participation avoids waiting a full PMU cycle budget on such CPUs.
const REPORT_WAIT_TIMEOUT: TimeSpan = TimeSpan::from_secs(5);
const MAX_REPORT_WAIT_POLLS: u32 = 50_000_000;

static WATCHED_CPUS: AtomicUsize = AtomicUsize::new(0);
static REPORT: ReportState = ReportState::new();
static SNAPSHOT: ktask::snapshot::TerminalSnapshot = ktask::snapshot::TerminalSnapshot::new();

const _: () = assert!(
    kbuild_config::NR_CPUS <= usize::BITS as usize,
    "watchdog report mask cannot represent all configured CPUs"
);

/// Registers this pinned CPU only after its periodic source was armed. The
/// reporter freezes membership when elected; subsequent registrations cannot
/// extend an active wait. Source teardown is terminal for watchdog today.
pub(crate) fn register_cpu() {
    if let Some(bit) = cpu_bit(this_cpu_id().as_usize()) {
        WATCHED_CPUS.fetch_or(bit, Ordering::Release);
    }
}

pub(crate) fn is_active() -> bool {
    REPORT.is_active()
}

/// Called by local timer IRQs and periodic NMIs. Nested entry on a collecting
/// CPU returns immediately; a newly admitted remote CPU publishes once and
/// parks without waiting for the reporter. It never resumes normal execution.
pub(crate) fn participate() {
    let cpu = this_cpu_id().as_usize();
    let Some(participation) = REPORT.claim(cpu) else {
        return;
    };
    khal::quiesce_nmi();
    if matches!(participation, Participation::Capture) {
        SNAPSHOT.collect_local();
        REPORT.mark_ready(cpu);
    }
    if !REPORT.is_owner(cpu) {
        karch::stop_cpu();
    }
}

/// Elects the first failing CPU and halts after a bounded collection attempt.
/// Losers only join the existing report; ordinary snapshots cannot suppress
/// this independent terminal snapshot. Called only from the watchdog NMI.
pub(crate) fn trigger(failed_task: &'static str) {
    let cpu = this_cpu_id().as_usize();
    if !REPORT.try_begin(cpu, WATCHED_CPUS.load(Ordering::Acquire)) {
        participate();
        return;
    }
    khal::kprint_atomic!("\n[watchdog] failure on cpu={cpu}, failed_task={failed_task}\n");
    let _ = khal::nmi::request_peer_samples();
    participate();

    let mut budget = WaitBudget::new(
        khal::time::monotonic_time().as_nanos_u64_saturating(),
        REPORT_WAIT_TIMEOUT.as_nanos() as u64,
        MAX_REPORT_WAIT_POLLS,
    );
    while !REPORT.all_ready() {
        if budget.is_exhausted(khal::time::monotonic_time().as_nanos_u64_saturating()) {
            break;
        }
        core::hint::spin_loop();
    }
    let collected = REPORT.close();
    let targets = REPORT.targets();
    khal::kprint_atomic!(
        "[watchdog] targets={targets:#x}, collected={collected:#x}, missing={:#x}\n",
        targets & !collected,
    );
    // SAFETY: ready publication follows each captured frame. Remote collectors
    // park before returning to scheduler/task execution; the owner's NMI frame
    // stays on this stack. Only those retained stacks are selected, and terminal
    // storage is never cleared or reused even when another collector is late.
    unsafe { SNAPSHOT.dump(collected) };
    khal::kprint_atomic!("[watchdog] fatal report complete; halting reporter cpu={cpu}\n");
    // The normal panic and platform halt paths may log through ordinary console
    // locks. A failed CPU may hold those locks, so use the bare CPU stop instead.
    karch::stop_cpu();
}
