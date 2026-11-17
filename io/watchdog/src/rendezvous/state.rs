// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One-shot fatal report admission, independent of architecture and clocks.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const NO_OWNER: usize = usize::MAX;

pub(super) enum Participation {
    Capture,
    Park,
}

/// A terminal protocol: ownership, target membership and published frames are
/// never reset. Acquiring `targets` observes owner election; acquiring `ready`
/// observes the corresponding CPU's completed snapshot publication. `claimed`
/// only suppresses duplicate/nested entry and carries no snapshot payload.
pub(super) struct ReportState {
    owner: AtomicUsize,
    targets: AtomicUsize,
    claimed: AtomicUsize,
    ready: AtomicUsize,
    is_closed: AtomicBool,
}

impl ReportState {
    pub(super) const fn new() -> Self {
        Self {
            owner: AtomicUsize::new(NO_OWNER),
            targets: AtomicUsize::new(0),
            claimed: AtomicUsize::new(0),
            ready: AtomicUsize::new(0),
            is_closed: AtomicBool::new(false),
        }
    }

    pub(super) fn try_begin(&self, cpu: usize, targets: usize) -> bool {
        let Some(bit) = cpu_bit(cpu) else {
            return false;
        };
        // The owner id is the election itself, so an observer never sees a
        // request with an unpublished reporter id. A peer seeing targets=0
        // simply retries at its next IRQ/NMI instead of waiting for setup.
        if self
            .owner
            .compare_exchange(NO_OWNER, cpu, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.targets.store(targets | bit, Ordering::Release);
        true
    }

    pub(super) fn is_active(&self) -> bool {
        self.owner.load(Ordering::Acquire) != NO_OWNER
    }

    pub(super) fn is_owner(&self, cpu: usize) -> bool {
        self.owner.load(Ordering::Acquire) == cpu
    }

    pub(super) fn targets(&self) -> usize {
        self.targets.load(Ordering::Acquire)
    }

    pub(super) fn claim(&self, cpu: usize) -> Option<Participation> {
        let bit = cpu_bit(cpu)?;
        if self.targets() & bit == 0 || self.claimed.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
            // In particular, an NMI interrupting a timer-IRQ collector must
            // return so the interrupted collector can finish publishing.
            return None;
        }
        Some(if self.is_closed.load(Ordering::Acquire) {
            Participation::Park
        } else {
            Participation::Capture
        })
    }

    pub(super) fn mark_ready(&self, cpu: usize) {
        if let Some(bit) = cpu_bit(cpu) {
            self.ready.fetch_or(bit, Ordering::Release);
        }
    }

    pub(super) fn ready(&self) -> usize {
        self.ready.load(Ordering::Acquire)
    }

    pub(super) fn all_ready(&self) -> bool {
        let targets = self.targets();
        targets != 0 && self.ready() & targets == targets
    }

    /// Only the reporter closes admission. An already admitted publisher may
    /// finish later, but the returned mask is final for this report and its
    /// slots are never reclaimed or reused.
    pub(super) fn close(&self) -> usize {
        self.is_closed.store(true, Ordering::Release);
        self.ready()
    }
}

pub(super) fn cpu_bit(cpu: usize) -> Option<usize> {
    1usize.checked_shl(u32::try_from(cpu).ok()?)
}

/// A poll cap also terminates the wait when the diagnostic clock stops or
/// regresses. It is an iteration bound, not a substitute wall-clock estimate.
pub(super) struct WaitBudget {
    start_ns: u64,
    timeout_ns: u64,
    polls_remaining: u32,
}

impl WaitBudget {
    pub(super) const fn new(start_ns: u64, timeout_ns: u64, max_polls: u32) -> Self {
        Self {
            start_ns,
            timeout_ns,
            polls_remaining: max_polls,
        }
    }

    pub(super) fn is_exhausted(&mut self, now_ns: u64) -> bool {
        if now_ns.saturating_sub(self.start_ns) >= self.timeout_ns || self.polls_remaining == 0 {
            return true;
        }
        self.polls_remaining -= 1;
        false
    }
}
