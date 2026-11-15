// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cgroup `cpu.max` bandwidth bridge between scheduler ticks and tasks.
//!
//! [`account_current`] charges the running task's cgroup tracker. When a
//! throttle window opens, every known member of that tracker is *parked*: the
//! entity stays linked in its ready queue but the fair scheduler never
//! selects it, matching Linux's cgroup bandwidth throttle where throttled
//! tasks leave the run queue until the period rolls over. [`poll_idle_bandwidth`]
//! runs on every account point — even for idle runners — so windows close and
//! members resume without requiring one of them to be running.
//!
//! A member joins the registry on its first charged tick. A task created (or
//! migrated) inside an already-throttled window is parked on that first tick,
//! so it cannot run for more than one accounting quantum past the throttle
//! boundary.

use alloc::{sync::Arc, vec::Vec};

use kbandwidth::Bandwidth;
use kspin::{SpinNoIrq, static_lock};

use crate::{KtaskRef, WeakKtaskRef};

static_lock! {
    /// Bandwidth trackers seen by the scheduler. Entries persist; groups
    /// without limits cost one atomic load per account point.
    static TRACKED: SpinNoIrq<Vec<Arc<Bandwidth>>> = SpinNoIrq::new(Vec::new());
}

static_lock! {
    /// Known members per tracker. Weak references: a task that exits must not
    /// be pinned by accounting state.
    static MEMBERS: SpinNoIrq<Vec<Member>> = SpinNoIrq::new(Vec::new());
}

struct Member {
    tracker: Arc<Bandwidth>,
    task: WeakKtaskRef,
}

/// Charges `elapsed_ns` of the running task's cgroup bandwidth.
///
/// Returns `true` when the running task is (or just became) parked and must be
/// preempted now. Idle runners poll the trackers instead so throttle windows
/// close without a member task running.
pub(crate) fn account_current(
    current: &KtaskRef,
    elapsed_ns: u64,
    now_ns: u64,
    is_idle: bool,
) -> bool {
    if is_idle {
        poll_trackers(now_ns);
        return false;
    }
    let Some(tracker) = current.cpu_bandwidth() else {
        poll_trackers(now_ns);
        return false;
    };
    register_tracker_once(&tracker);
    register_member(&tracker, current);

    // Close an expired window before charging the new period. `charge()`
    // rolls the period over, so polling afterwards would observe zero elapsed
    // time and never resume members parked in the previous window.
    if tracker.poll(now_ns) {
        resume_group(&tracker);
    }
    let started_throttle = tracker.charge(elapsed_ns / 1_000, now_ns);
    if started_throttle || tracker.is_throttled() {
        park_group(&tracker, now_ns);
    }
    current.is_cgroup_parked()
}

/// Runs the lazy period rollover for every tracked group and resumes members
/// whose window closed.
pub(crate) fn poll_trackers(now_ns: u64) {
    let mut resumed = Vec::new();
    for tracker in TRACKED.lock().iter() {
        if tracker.poll(now_ns) {
            resumed.push(tracker.clone());
        }
    }
    for tracker in &resumed {
        resume_group(tracker);
    }
    if !resumed.is_empty() {
        MEMBERS
            .lock()
            .retain(|member| member.task.upgrade().is_some());
    }
}

fn register_tracker_once(tracker: &Arc<Bandwidth>) {
    let mut tracked = TRACKED.lock();
    if !tracked.iter().any(|entry| Arc::ptr_eq(entry, tracker)) {
        tracked.push(tracker.clone());
    }
}

fn register_member(tracker: &Arc<Bandwidth>, task: &KtaskRef) {
    let mut members = MEMBERS.lock();
    let known = members.iter().any(|member| {
        Arc::ptr_eq(&member.tracker, tracker)
            && member
                .task
                .upgrade()
                .is_some_and(|known| KtaskRef::ptr_eq(&known, task))
    });
    if !known {
        members.push(Member {
            tracker: tracker.clone(),
            task: KtaskRef::downgrade(task),
        });
    }
}

/// Parks every selectable member of `tracker` until the period rolls over.
fn park_group(tracker: &Arc<Bandwidth>, _now_ns: u64) {
    for member in MEMBERS.lock().iter() {
        if !Arc::ptr_eq(&member.tracker, tracker) {
            continue;
        }
        if let Some(task) = member.task.upgrade() {
            task.set_cgroup_parked(true);
            // Force a scheduling point so a running member stops at the next
            // timer/syscall boundary instead of finishing its slice.
            task.set_preempt_pending(true);
        }
    }
}

/// Makes every member of `tracker` selectable again.
fn resume_group(tracker: &Arc<Bandwidth>) {
    for member in MEMBERS.lock().iter() {
        if !Arc::ptr_eq(&member.tracker, tracker) {
            continue;
        }
        if let Some(task) = member.task.upgrade() {
            task.set_cgroup_parked(false);
            task.set_preempt_pending(true);
        }
    }
}

/// Forgets a task's membership at exit.
pub(crate) fn forget_member(task: &KtaskRef) {
    MEMBERS.lock().retain(|member| {
        member
            .task
            .upgrade()
            .is_some_and(|known| !KtaskRef::ptr_eq(&known, task))
    });
}
