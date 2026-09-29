// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fixed-priority realtime class: FIFO and RR share one ready array.
//!
//! Each priority `1..=99` has an intrusive FIFO list. A 128-bit bitmap finds
//! the highest occupied level in O(1). Same priority is list order. RR adds
//! a leftover quantum. Period advancement uses the absolute [`RqClock`].

use alloc::sync::Arc;

use crate::{
    DEFAULT_RR_SLICE_NS, DEFAULT_RT_PERIOD_NS, DEFAULT_RT_RUNTIME_NS,
    class::{ClassOps, EnqueueKind, LeaveReason},
    clock::RqClock,
    config::{MIN_RT_PRIORITY, RT_PRIO_LEVELS},
    entity::{QueueSeat, RqLocked, SchedTask},
    policy::{SchedAttr, SchedClass, SchedError, SchedPolicy},
    ready_list::ReadyList,
};

/// Per-CPU RT ready queue shared by FIFO and RR.
pub(crate) struct RtClass<T> {
    queues: [ReadyList<SchedTask<T>>; RT_PRIO_LEVELS],
    occupied: u128,
    queued: usize,
    used_ns: u64,
    period_start_ns: u64,
    is_throttled: bool,
    /// The running RR task finished its quantum and should requeue at the tail
    /// if this preemption is the one `account` just requested.
    rr_rotate_curr: bool,
    rr_slice_ns: u64,
    rt_runtime_ns: u64,
    rt_period_ns: u64,
}

impl<T> RtClass<T> {
    pub const fn new() -> Self {
        Self {
            queues: [const { ReadyList::new() }; RT_PRIO_LEVELS],
            occupied: 0,
            queued: 0,
            used_ns: 0,
            period_start_ns: 0,
            is_throttled: false,
            rr_rotate_curr: false,
            rr_slice_ns: DEFAULT_RR_SLICE_NS,
            rt_runtime_ns: DEFAULT_RT_RUNTIME_NS,
            rt_period_ns: DEFAULT_RT_PERIOD_NS,
        }
    }

    fn idx(priority: u8) -> usize {
        (priority - MIN_RT_PRIORITY) as usize
    }

    fn is_occupied(&self, priority: u8) -> bool {
        self.occupied & (1u128 << Self::idx(priority)) != 0
    }

    fn mark_occupied(&mut self, priority: u8) {
        self.occupied |= 1u128 << Self::idx(priority);
    }

    fn mark_empty_if(&mut self, priority: u8) {
        if self.queues[Self::idx(priority)].is_empty() {
            self.occupied &= !(1u128 << Self::idx(priority));
        }
    }

    fn highest_ready_prio(&self) -> Option<u8> {
        if self.occupied == 0 {
            return None;
        }
        let bit = 127 - self.occupied.leading_zeros();
        Some((bit as u8) + MIN_RT_PRIORITY)
    }

    fn has_same_prio_waiter(&self, priority: u8) -> bool {
        (MIN_RT_PRIORITY..=99).contains(&priority) && self.is_occupied(priority)
    }

    fn enqueue_ready(&mut self, task: Arc<SchedTask<T>>, front: bool, tok: RqLocked) {
        let priority = task.rt_priority(tok).get();
        debug_assert!((MIN_RT_PRIORITY..=99).contains(&priority));
        task.set_seat(QueueSeat::Rt, tok);
        if front {
            self.queues[Self::idx(priority)].push_front(task);
        } else {
            self.queues[Self::idx(priority)].push_back(task);
        }
        self.queued += 1;
        self.mark_occupied(priority);
    }

    fn enqueue_tail(&mut self, task: Arc<SchedTask<T>>, tok: RqLocked) {
        self.enqueue_ready(task, false, tok);
    }

    fn enqueue_head(&mut self, task: Arc<SchedTask<T>>, tok: RqLocked) {
        self.enqueue_ready(task, true, tok);
    }

    fn dequeue_addr(
        &mut self,
        priority: u8,
        task: &SchedTask<T>,
        tok: RqLocked,
    ) -> Option<Arc<SchedTask<T>>> {
        if !(MIN_RT_PRIORITY..=99).contains(&priority) || !self.is_occupied(priority) {
            return None;
        }
        let list = &mut self.queues[Self::idx(priority)];
        let removed = list.remove(task)?;
        removed.set_seat(QueueSeat::Detached, tok);
        self.queued = self.queued.saturating_sub(1);
        self.mark_empty_if(priority);
        Some(removed)
    }

    fn ns_until_throttle(&self) -> u64 {
        self.rt_runtime_ns.saturating_sub(self.used_ns)
    }
}

impl<T> ClassOps<T> for RtClass<T> {
    fn class(&self) -> SchedClass {
        SchedClass::Rt
    }

    fn nr_queued(&self) -> usize {
        self.queued
    }

    fn has_runnable(&self) -> bool {
        self.occupied != 0 && !self.is_throttled
    }

    fn advance(&mut self, clk: &RqClock, _tok: RqLocked) {
        let now = clk.now_ns();
        if self.period_start_ns == 0 {
            self.period_start_ns = now;
        }
        let period = self.rt_period_ns.max(1);
        if now >= self.period_start_ns.saturating_add(period) {
            let steps = now.saturating_sub(self.period_start_ns) / period;
            self.period_start_ns = self
                .period_start_ns
                .saturating_add(steps.saturating_mul(period));
            self.used_ns = 0;
            self.is_throttled = false;
        }
    }

    fn enqueue(
        &mut self,
        task: Arc<SchedTask<T>>,
        kind: EnqueueKind,
        _curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        tok: RqLocked,
    ) {
        if kind == EnqueueKind::New && task.policy(tok) == SchedPolicy::Rr {
            task.rt().rr_left_ns.set(self.rr_slice_ns, tok);
        }
        self.enqueue_tail(task, tok);
    }

    fn dequeue(
        &mut self,
        task: &SchedTask<T>,
        _curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> Option<Arc<SchedTask<T>>> {
        if task.seat(tok) != QueueSeat::Rt {
            return None;
        }
        self.dequeue_addr(task.rt_priority(tok).get(), task, tok)
    }

    fn steal(
        &mut self,
        want: &mut dyn FnMut(&SchedTask<T>) -> bool,
        _curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> Option<Arc<SchedTask<T>>> {
        let highest = self.highest_ready_prio()?;
        for prio in (MIN_RT_PRIORITY..=highest).rev() {
            if !self.is_occupied(prio) {
                continue;
            }
            let list = &mut self.queues[Self::idx(prio)];
            let Some(task) = list.steal(|queued| want(queued)) else {
                continue;
            };
            task.set_seat(QueueSeat::Detached, tok);
            self.queued = self.queued.saturating_sub(1);
            self.mark_empty_if(prio);
            return Some(task);
        }
        None
    }

    fn pick_next(&mut self, _clk: &RqClock, tok: RqLocked) -> Option<Arc<SchedTask<T>>> {
        if self.is_throttled {
            return None;
        }
        let prio = self.highest_ready_prio()?;
        let task = self.queues[Self::idx(prio)].pop_front()?;
        self.queued = self.queued.saturating_sub(1);
        self.mark_empty_if(prio);
        task.set_seat(QueueSeat::Detached, tok);
        if task.policy(tok) == SchedPolicy::Rr && task.rt().rr_left_ns.get(tok) == 0 {
            task.rt().rr_left_ns.set(self.rr_slice_ns, tok);
        }
        Some(task)
    }

    fn set_curr(&mut self, _task: &SchedTask<T>, _clk: &RqClock, _tok: RqLocked) {}

    fn put_prev(
        &mut self,
        prev: Arc<SchedTask<T>>,
        reason: LeaveReason,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> bool {
        match reason {
            LeaveReason::Yield => {
                self.rr_rotate_curr = false;
                self.enqueue_tail(prev, tok);
                true
            }
            LeaveReason::Preempt => {
                if self.rr_rotate_curr {
                    self.rr_rotate_curr = false;
                    self.enqueue_tail(prev, tok);
                } else {
                    self.enqueue_head(prev, tok);
                }
                true
            }
            // Block, migrate, and exit stay off this CPU. The destination
            // enqueues a migrate with `EnqueueKind::MigrateIn`.
            LeaveReason::Block | LeaveReason::Migrate | LeaveReason::Exit => {
                self.rr_rotate_curr = false;
                false
            }
        }
    }

    fn account(
        &mut self,
        curr: &SchedTask<T>,
        elapsed_ns: u64,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> bool {
        if elapsed_ns == 0 {
            return false;
        }
        self.used_ns = self.used_ns.saturating_add(elapsed_ns);
        if self.used_ns >= self.rt_runtime_ns {
            self.is_throttled = true;
        }
        let mut expired = false;
        if curr.policy(tok) == SchedPolicy::Rr {
            let old = curr.rt().rr_left_ns.get(tok);
            let left = old.saturating_sub(elapsed_ns.min(old));
            if left == 0 {
                // Refill even with no peer, so a later same-priority wake does
                // not find a zero slice and rotate immediately.
                curr.rt().rr_left_ns.set(self.rr_slice_ns, tok);
                let rotate = self.has_same_prio_waiter(curr.rt_priority(tok).get());
                self.rr_rotate_curr = rotate;
                expired = rotate;
            } else {
                curr.rt().rr_left_ns.set(left, tok);
                self.rr_rotate_curr = false;
            }
        }
        expired || self.is_throttled
    }

    fn tick_preempts(&mut self, curr: &SchedTask<T>, clk: &RqClock, tok: RqLocked) -> bool {
        self.wakeup_preempts(curr, clk, tok)
    }

    fn wakeup_preempts(&mut self, curr: &SchedTask<T>, _clk: &RqClock, tok: RqLocked) -> bool {
        if self.is_throttled {
            return false;
        }
        match self.highest_ready_prio() {
            Some(highest) => highest > curr.rt_priority(tok).get(),
            None => false,
        }
    }

    fn curr_timer_ns(&self, curr: &SchedTask<T>, _clk: &RqClock, tok: RqLocked) -> Option<u64> {
        if self.is_throttled {
            return None;
        }
        let mut next = self.ns_until_throttle();
        if curr.policy(tok) == SchedPolicy::Rr
            && self.has_same_prio_waiter(curr.rt_priority(tok).get())
        {
            next = next.min(curr.rt().rr_left_ns.get(tok));
        }
        Some(next)
    }

    fn class_timer_ns(&self, clk: &RqClock, _tok: RqLocked) -> Option<u64> {
        if !self.is_throttled {
            return None;
        }
        Some(
            self.period_start_ns
                .saturating_add(self.rt_period_ns)
                .saturating_sub(clk.now_ns()),
        )
    }

    fn attr_changed(
        &mut self,
        task: &Arc<SchedTask<T>>,
        old: SchedAttr,
        is_curr: bool,
        _clk: &RqClock,
        tok: RqLocked,
    ) {
        let new_prio = task.rt_priority(tok).get();
        let entered_rr = old.policy != SchedPolicy::Rr && task.policy(tok) == SchedPolicy::Rr;
        if entered_rr {
            task.rt().rr_left_ns.set(self.rr_slice_ns, tok);
        }
        if is_curr {
            return;
        }
        if old.rt_priority.get() != new_prio
            && let Some(removed) = self.dequeue_addr(old.rt_priority.get(), task.as_ref(), tok)
        {
            // A lowered priority goes to the head of its new level so tasks
            // already waiting there do not run ahead of it.
            if new_prio < old.rt_priority.get() {
                self.enqueue_head(removed, tok);
            } else {
                self.enqueue_tail(removed, tok);
            }
        }
    }

    fn can_admit(
        &self,
        _task: &SchedTask<T>,
        _new: SchedAttr,
        _tok: RqLocked,
    ) -> Result<(), SchedError> {
        // `SchedAttr::validate` already rejected a bad FIFO/RR priority and
        // Deadline. This class has no bandwidth admission.
        Ok(())
    }

    fn switched_from(
        &mut self,
        _task: &SchedTask<T>,
        _is_curr: bool,
        _clk: &RqClock,
        _tok: RqLocked,
    ) {
    }

    fn switched_to(&mut self, task: &SchedTask<T>, _is_curr: bool, _clk: &RqClock, tok: RqLocked) {
        if task.policy(tok) == SchedPolicy::Rr && task.rt().rr_left_ns.get(tok) == 0 {
            task.rt().rr_left_ns.set(self.rr_slice_ns, tok);
        }
    }
}
