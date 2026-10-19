// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::Ordering;

use kerrno::{KError, KResult};
use kidentity::TaskIdentityToken;
use kspin::SpinNoIrq;
use ktask::{KtaskRef, WeakKtaskRef};
use ktime_types::TimeSpan;

use super::Process;
use crate::{AsThread, Tid, process_domain, publication::process_publication};

/// Published tasks and unreleased group-leader membership tracked per process.
///
/// Members are keyed by the task's stable [`TaskIdentityToken`], not by its
/// current TID, so an `exec` leader takeover that moves a number does not have
/// to re-index or migrate membership.
#[derive(Default)]
pub(super) struct ThreadMembership {
    members: BTreeMap<TaskIdentityToken, Arc<ThreadMemberSlot>>,
    pub(super) exited_utime: TimeSpan,
    pub(super) exited_stime: TimeSpan,
    sum_exec_runtime_ns: u64,
    pub(super) child_utime: TimeSpan,
    pub(super) child_stime: TimeSpan,
}

impl ThreadMembership {
    pub(super) fn reserve_slot(&mut self, token: TaskIdentityToken) -> Arc<ThreadMemberSlot> {
        // Runtime kidentity allocation is monotonic and checked: numbers are
        // never recycled on exit/drop or wrapped at exhaustion. Fixed-number
        // replacement fixtures exercise stale identity guards, not allocation.
        self.members
            .entry(token)
            .or_insert_with(|| Arc::new(ThreadMemberSlot::new()))
            .clone()
    }

    fn exit_task(&mut self, task: &KtaskRef, is_leader: bool) -> bool {
        let Some(slot) = self
            .members
            .get(&task.as_thread().identity_token())
            .cloned()
        else {
            return false;
        };
        if !slot.exit_task(task, is_leader) {
            return false;
        }
        if !is_leader {
            self.accumulate_task(task);
            self.members.remove(&task.as_thread().identity_token());
        }
        !self.members.values().any(|slot| slot.is_published())
    }

    fn accumulate_task(&mut self, task: &KtaskRef) {
        let (utime, stime) = task.as_thread().sample_cpu_time();
        self.exited_utime = self.exited_utime.saturating_add(utime);
        self.exited_stime = self.exited_stime.saturating_add(stime);
        self.sum_exec_runtime_ns = self
            .sum_exec_runtime_ns
            .saturating_add(task.sum_exec_runtime_ns());
    }

    pub(super) fn cpu_times(&self) -> (TimeSpan, TimeSpan, u64) {
        self.members
            .values()
            .filter_map(|slot| slot.accounting_task())
            .fold(
                (
                    self.exited_utime,
                    self.exited_stime,
                    self.sum_exec_runtime_ns,
                ),
                |(user, system, runtime), task| {
                    let (utime, stime) = task.as_thread().sample_cpu_time();
                    (
                        user.saturating_add(utime),
                        system.saturating_add(stime),
                        runtime.saturating_add(task.sum_exec_runtime_ns()),
                    )
                },
            )
    }

    fn has_no_published_tasks(&self) -> bool {
        self.members.values().all(|slot| slot.snapshot().is_none())
    }

    fn unreleased_count(&self) -> usize {
        self.members
            .values()
            .filter(|slot| slot.is_unreleased())
            .count()
    }

    fn release_exited_leader(&mut self) -> Option<Arc<ThreadMemberSlot>> {
        let token = self
            .members
            .iter()
            .find_map(|(token, slot)| slot.is_exited_leader().then_some(*token))?;
        let task = self.members.get(&token)?.accounting_task()?;
        self.accumulate_task(&task);
        self.members.remove(&token)
    }

    fn thread_ids(&self) -> Vec<Tid> {
        self.members
            .values()
            .filter_map(|slot| slot.snapshot())
            .map(|task| task.as_thread().tid())
            .collect()
    }

    fn tasks(&self) -> Vec<KtaskRef> {
        self.members
            .values()
            .filter_map(|slot| slot.snapshot())
            .collect()
    }

    fn published_count(&self) -> usize {
        self.members
            .values()
            .filter(|slot| slot.snapshot().is_some())
            .count()
    }

    fn contains_published_tid(&self, tid: Tid) -> bool {
        self.members
            .values()
            .filter_map(|slot| slot.snapshot())
            .any(|task| task.as_thread().tid() == tid)
    }

    fn representative_task(&self) -> KResult<KtaskRef> {
        self.members
            .values()
            .find_map(|slot| slot.snapshot())
            .ok_or(KError::NoSuchProcess)
    }
}

/// Thread-group exit state tracked per process.
#[derive(Default)]
pub(super) struct ThreadGroupExitState {
    pub(super) exit_code: i32,
    pub(super) group_exited: bool,
}

enum ThreadMemberState {
    Vacant,
    Published(WeakKtaskRef),
    // Retain the leader's final counters until wait/autoreap, without allowing
    // TID lookup or scheduling through the membership table.
    ExitedLeader(KtaskRef),
}

pub(crate) struct ThreadMemberSlot {
    state: SpinNoIrq<ThreadMemberState>,
}

impl ThreadMemberSlot {
    fn new() -> Self {
        Self {
            state: SpinNoIrq::new(ThreadMemberState::Vacant),
        }
    }

    fn publish(&self, task: &KtaskRef) {
        *self.state.lock() = ThreadMemberState::Published(Arc::downgrade(task));
    }

    fn retire(&self) {
        *self.state.lock() = ThreadMemberState::Vacant;
    }

    fn exit_task(&self, task: &KtaskRef, is_leader: bool) -> bool {
        let mut state = self.state.lock();
        let ThreadMemberState::Published(published) = &*state else {
            return false;
        };
        if !Weak::ptr_eq(published, &Arc::downgrade(task)) {
            return false;
        }
        *state = if is_leader {
            ThreadMemberState::ExitedLeader(task.clone())
        } else {
            ThreadMemberState::Vacant
        };
        true
    }

    fn is_published(&self) -> bool {
        matches!(&*self.state.lock(), ThreadMemberState::Published(task) if task.strong_count() != 0)
    }

    fn accounting_task(&self) -> Option<KtaskRef> {
        match &*self.state.lock() {
            ThreadMemberState::Published(task) => task.upgrade(),
            ThreadMemberState::ExitedLeader(task) => Some(task.clone()),
            ThreadMemberState::Vacant => None,
        }
    }

    fn is_unreleased(&self) -> bool {
        match &*self.state.lock() {
            ThreadMemberState::Vacant => false,
            ThreadMemberState::Published(task) => task.strong_count() != 0,
            ThreadMemberState::ExitedLeader(_) => true,
        }
    }

    fn is_exited_leader(&self) -> bool {
        matches!(*self.state.lock(), ThreadMemberState::ExitedLeader(_))
    }

    pub(super) fn snapshot(&self) -> Option<KtaskRef> {
        match &*self.state.lock() {
            ThreadMemberState::Published(task) => {
                task.upgrade().filter(|task| !task.as_thread().is_exiting())
            }
            ThreadMemberState::Vacant | ThreadMemberState::ExitedLeader(_) => None,
        }
    }
}

impl Process {
    pub(crate) fn reserve_thread_member_slot(
        &self,
        token: TaskIdentityToken,
    ) -> Arc<ThreadMemberSlot> {
        self.thread_membership.lock().reserve_slot(token)
    }

    /// Moves the thread-group identity onto `task` after a non-leader `exec`.
    ///
    /// `task` must be the process's sole surviving published thread. The number
    /// directory entry moves once and the thread adopts the group number;
    /// membership and cgroup key on the stable identity and need no re-keying.
    /// The former leader is released and its counters join the group totals.
    /// Signal routing still keys threads by number, so its registry entry is
    /// re-keyed here. Call in sleepable context, before the new image is
    /// installed; returns `false` when the precondition or directory move fails.
    pub fn assume_leader_identity(&self, task: &KtaskRef) -> bool {
        let thread = task.as_thread();
        let old_tid = thread.tid();
        let pid = self.pid();
        // Verify the sole-survivor invariant for the leader case too, so a
        // sibling published during the sweep is refused instead of being left
        // running against the old address space.
        if self.thread_count() != 1 || !self.contains_published_tid(old_tid) {
            return false;
        }
        if old_tid == pid {
            return true;
        }
        if !process_publication().move_task_number(task, old_tid, pid) {
            return false;
        }
        thread.adopt_task_number(self.leader_task_number.clone());
        thread.signal_manager().rebind_after_exec(pid);
        // Identity takeover ends the old leader's accounting membership;
        // retaining it until wait would keep obsolete leaders across exec.
        self.release_exited_thread_members();
        true
    }

    /// Publishes a thread task into this [`Process`]'s membership table.
    pub(crate) fn publish_thread_task_locked(
        &self,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
        slot: &Arc<ThreadMemberSlot>,
        task: &KtaskRef,
    ) {
        let _membership = self.thread_membership.lock();
        slot.publish(task);
    }

    pub(crate) fn rollback_thread_member(&self, slot: &Arc<ThreadMemberSlot>, task: &KtaskRef) {
        let mut membership = self.thread_membership.lock();
        if !task.as_thread().is_exiting()
            && slot.is_published()
            && membership
                .members
                .get(&task.as_thread().identity_token())
                .is_some_and(|owned| Arc::ptr_eq(owned, slot))
            && slot
                .accounting_task()
                .is_some_and(|published| Arc::ptr_eq(&published, task))
        {
            slot.retire();
            membership
                .members
                .remove(&task.as_thread().identity_token());
        }
    }

    /// Ends external TID visibility while keeping the task in group accounting.
    /// Captures the MM pin before cleanup; no user access is allowed afterwards.
    /// Returns whether this was the last thread to begin exit.
    pub(crate) fn begin_thread_exit(self: &Arc<Self>, task: &KtaskRef, exit_code: i32) -> bool {
        task.as_thread().capture_exit_address_space();
        let is_last = {
            let membership = self.thread_membership.lock();
            // Check the real boundary, including direct kprocess test callers.
            #[cfg(unittest)]
            assert!(task.as_thread().has_exit_address_space());
            let matches = membership
                .members
                .get(&task.as_thread().identity_token())
                .and_then(|slot| slot.snapshot())
                .is_some_and(|published| Arc::ptr_eq(&published, task));
            if !matches {
                return false;
            }
            {
                let mut group_exit = self.group_exit.lock();
                if !group_exit.group_exited && !task.as_thread().is_exec_sweep_target() {
                    group_exit.exit_code = exit_code;
                }
            }
            // Signal eligibility ends with live membership, before cleanup.
            task.as_thread().signal_manager().exit();
            task.as_thread().set_exit();
            membership.has_no_published_tasks()
        };
        process_publication().unpublish_task_if_matches(task);
        is_last
    }

    /// Commits completed cleanup and transfers a nonleader's counters exactly once.
    /// The leader remains an accounting member until process identity release.
    /// Returns true only for the last successful cleanup completion.
    pub(crate) fn exit_thread(self: &Arc<Self>, task: &KtaskRef, exit_code: i32) -> bool {
        if !task.as_thread().is_exiting() {
            self.begin_thread_exit(task, exit_code);
        }
        if ktask::current_may_uninit().is_some_and(|current| current.ptr_eq(task)) {
            ktask::refresh_current_execution_runtime_ns();
        }
        self.thread_membership
            .lock()
            .exit_task(task, task.as_thread().tid() == self.pid())
    }

    /// Returns a snapshot of published thread IDs in this [`Process`].
    pub fn threads(&self) -> Vec<Tid> {
        self.thread_membership.lock().thread_ids()
    }

    /// Returns a snapshot of published thread tasks in this [`Process`].
    pub(crate) fn thread_tasks(&self) -> Vec<KtaskRef> {
        self.thread_membership.lock().tasks()
    }

    /// Returns the number of published threads currently attached to this [`Process`].
    pub(crate) fn thread_count(&self) -> usize {
        self.thread_membership.lock().published_count()
    }

    /// Counts published members plus an exited leader not yet released by reap.
    pub(crate) fn unreleased_thread_count(&self) -> usize {
        self.thread_membership.lock().unreleased_count()
    }

    /// Releases retained leader accounting after identity retirement.
    /// Call without the process-domain lock: the final task reference can run
    /// runtime destructors and is dropped outside both accounting locks.
    pub(crate) fn release_exited_thread_members(&self) {
        loop {
            let retired = self.thread_membership.lock().release_exited_leader();
            if retired.is_none() {
                break;
            }
            // Drop the retained task only after releasing the membership lock.
            drop(retired);
        }
    }

    /// Returns whether `tid` still resolves to a published thread in this [`Process`].
    pub(crate) fn contains_published_tid(&self, tid: Tid) -> bool {
        self.thread_membership.lock().contains_published_tid(tid)
    }

    /// Returns the lowest-TID published task that currently represents this [`Process`].
    pub(crate) fn representative_task(&self) -> KResult<KtaskRef> {
        self.thread_membership.lock().representative_task()
    }

    /// Returns `true` if the [`Process`] is group exited.
    pub fn is_group_exited(&self) -> bool {
        self.group_exit.lock().group_exited
    }

    /// Marks the [`Process`] as group exited.
    pub(crate) fn group_exit(&self) {
        self.group_exit.lock().group_exited = true;
    }

    /// The parent-death signal recorded by `prctl(PR_SET_PDEATHSIG)` — the
    /// signal this process should receive when its parent dies first. Zero
    /// when unset. Storage only: the parent-exit delivery path is not wired
    /// up yet (see `core/ksyscall/docs/security.md`).
    pub fn pdeath_signal(&self) -> u32 {
        self.pdeath_signal.load(Ordering::Relaxed)
    }

    /// Records the parent-death signal for this process.
    pub fn set_pdeath_signal(&self, signo: u32) {
        self.pdeath_signal.store(signo, Ordering::Relaxed);
    }

    /// The exit code of the [`Process`].
    pub fn exit_code(&self) -> i32 {
        self.group_exit.lock().exit_code
    }
}
