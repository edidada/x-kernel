// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Rendezvous coordination for watchdog failures.
use core::sync::atomic::{AtomicUsize, Ordering};

use khal::percpu::this_cpu_id;
use ktime_types::TimeSpan;

/// Maximum time the cause CPU waits for peer CPUs to enter the snapshot NMI.
///
/// Linux's NMI backtrace rendezvous is also bounded: an unresponsive CPU must
/// be reported as missing instead of trapping every responder in NMI forever.
pub const WAIT_TIMEOUT: TimeSpan = TimeSpan::from_secs(10);

/// Rendezvous phases.
#[repr(usize)]
#[derive(Copy, Clone, Eq, PartialEq)]
enum Phase {
    /// No rendezvous in progress.
    Idle      = 0,
    /// Triggered: all CPUs must enter NMI and mark arrived.
    Triggered = 1,
    /// Dump done: non-cause CPUs can stop spinning if desired.
    DumpDone  = 2,
}

impl Phase {
    #[inline]
    fn load() -> Self {
        match PHASE.load(Ordering::Acquire) {
            0 => Phase::Idle,
            1 => Phase::Triggered,
            2 => Phase::DumpDone,
            _ => Phase::Idle,
        }
    }
}

static PHASE: AtomicUsize = AtomicUsize::new(Phase::Idle as usize);

/// The CPU id which detected the failure and triggered the rendezvous.
static CAUSE_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Per-cpu arrived bitmap stored in an AtomicUsize where bit i means CPU i arrived.
static ARRIVED_BITMAP: AtomicUsize = AtomicUsize::new(0);

/// CPUs that own a snapshot write, including writers not yet published as arrived.
static CLAIMED_BITMAP: AtomicUsize = AtomicUsize::new(0);

/// Returns whether a rendezvous is in progress.
#[inline]
pub fn is_triggered() -> bool {
    Phase::load() == Phase::Triggered
}

/// Returns whether the dump phase has completed.
#[inline]
pub fn is_dump_done() -> bool {
    Phase::load() == Phase::DumpDone
}

/// Try to trigger rendezvous.
///
/// The calling CPU becomes the cause CPU only if this call transitions the
/// rendezvous from idle to triggered.
#[inline]
pub fn try_trigger() -> bool {
    let cpu = this_cpu_id().as_usize();
    if PHASE
        .compare_exchange(
            Phase::Idle as usize,
            Phase::Triggered as usize,
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .is_ok()
    {
        CAUSE_CPU.store(cpu, Ordering::Release);
        true
    } else {
        false
    }
}

/// Returns the CPU that triggered the rendezvous.
#[inline]
pub fn cause_cpu() -> Option<usize> {
    if Phase::load() == Phase::Idle {
        return None;
    }
    let cpu = CAUSE_CPU.load(Ordering::Acquire);
    (cpu != usize::MAX).then_some(cpu)
}

fn try_claim_cpu(claimed: &AtomicUsize, cpu: usize) -> bool {
    if cpu >= usize::BITS as usize {
        return false;
    }
    let bit = 1usize << cpu;
    // Acquire keeps snapshot writes after the claim. Arrival publication is
    // separate: claiming a slot does not mean it is safe for remote readers.
    claimed.fetch_or(bit, Ordering::AcqRel) & bit == 0
}

/// Claims the current CPU's single snapshot write in an active rendezvous.
///
/// A nested NMI can interrupt a regular-IRQ backtrace responder before it
/// publishes arrival. A losing entry must return immediately, without writing,
/// publishing arrival, or waiting for the interrupted writer to finish.
/// Call from IRQ/NMI context, with the current CPU pinned through collection
/// and arrival publication.
pub fn try_join() -> bool {
    is_triggered() && try_claim_cpu(&CLAIMED_BITMAP, this_cpu_id().as_usize())
}

/// Publish that the current CPU has completed its local snapshot.
///
/// The caller must finish writing every datum consumed by the cause CPU before
/// this call. The Release operation pairs with [`arrived_bitmap`]'s Acquire
/// load, so observing this bit also makes the completed snapshot visible.
#[inline]
pub fn publish_arrived() {
    let id = this_cpu_id().as_usize();
    if id >= usize::BITS as usize {
        // Cannot represent this CPU in the bitmap without overflowing the shift.
        return;
    }
    ARRIVED_BITMAP.fetch_or(1usize << id, Ordering::Release);
}

pub fn arrived_bitmap() -> usize {
    ARRIVED_BITMAP.load(Ordering::Acquire)
}

pub fn all_arrived_mask() -> usize {
    // Cover only the CPUs the platform actually described (DT/ACPI), not the
    // compile-time `NR_CPUS` cap; otherwise the rendezvous would wait forever
    // for non-present CPUs to arrive.
    let n = kcpu_id_map::nr_cpus();
    if n >= usize::BITS as usize {
        usize::MAX
    } else {
        (1usize << n) - 1
    }
}

#[inline]
fn wait_finished(arrived: usize, expected: usize, elapsed: TimeSpan, timeout: TimeSpan) -> bool {
    arrived & expected == expected || elapsed >= timeout
}

/// Busy-wait for the expected CPUs, returning the bitmap that actually arrived.
///
/// This function is safe to call from the watchdog NMI path: it allocates
/// nothing and takes no locks. The bounded wait is a correctness requirement,
/// not merely a diagnostic policy; a CPU that cannot enter NMI must not keep
/// all responding CPUs trapped in the rendezvous forever.
pub fn wait_all_arrived(expected: usize, timeout: TimeSpan) -> usize {
    let start = khal::time::monotonic_time();
    loop {
        let arrived = arrived_bitmap();
        let elapsed = khal::time::monotonic_time().saturating_duration_since(start);
        if wait_finished(arrived, expected, elapsed, timeout) {
            return arrived;
        }
        core::hint::spin_loop();
    }
}

/// Mark dump done so other CPUs can release from spinning.
pub fn mark_dump_done() {
    PHASE.store(Phase::DumpDone as usize, Ordering::Release);
}

/// Reset rendezvous state.
///
/// Call this only after `mark_dump_done()` and after ensuring every responding
/// CPU observed it; otherwise a responder could miss the release transition
/// and continue spinning until the terminal panic path stops the machine.
#[inline]
pub fn reset() {
    ARRIVED_BITMAP.store(0, Ordering::Release);
    CLAIMED_BITMAP.store(0, Ordering::Release);
    CAUSE_CPU.store(usize::MAX, Ordering::Release);
    PHASE.store(Phase::Idle as usize, Ordering::Release);
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::*;

    #[def_test]
    fn nested_entry_cannot_claim_an_unpublished_snapshot() {
        let claimed = AtomicUsize::new(0);
        assert!(try_claim_cpu(&claimed, 0));
        // Simulate a PMU NMI preempting the regular-IRQ writer before its
        // snapshot is complete. It must neither overwrite nor acknowledge it.
        assert!(!try_claim_cpu(&claimed, 0));
        assert!(try_claim_cpu(&claimed, 1));
        assert!(!try_claim_cpu(&claimed, 0));
        assert!(!try_claim_cpu(&claimed, usize::BITS as usize));
    }

    #[def_test]
    fn wait_finishes_when_every_expected_cpu_arrived() {
        let timeout = TimeSpan::from_secs(10);
        assert!(wait_finished(0b111, 0b101, TimeSpan::ZERO, timeout));
    }

    #[def_test(serial)]
    fn wait_returns_after_timeout_when_a_cpu_never_arrives() {
        let timeout = TimeSpan::from_secs(10);
        assert!(!wait_finished(
            0b001,
            0b011,
            TimeSpan::from_secs(9),
            timeout
        ));
        assert!(wait_finished(
            0b001,
            0b011,
            TimeSpan::from_secs(10),
            timeout
        ));

        // Exercise the production loop against the real monotonic clock with
        // no published arrivals. Returning from this call is the regression
        // guarantee for a CPU that can never enter the rendezvous.
        reset();
        let expected = all_arrived_mask();
        let arrived = wait_all_arrived(expected, TimeSpan::from_millis(1));
        assert!(arrived & expected != expected);
    }
}
