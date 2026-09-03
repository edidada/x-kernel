// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unlocked bottom-half worker-pool state machine.

use ktime_types::MonotonicInstant;

use super::{
    candidate::{
        BottomHalfRunnableCandidate, BottomHalfRunnableCandidateDiscard,
        BottomHalfRunnableCandidateResult,
    },
    slot::BottomHalfDrainSlot,
};
use crate::{
    EntryKey, EntryOwner, EntryPayload, ParkDecision, PoolEntry, QueueRemoveResult,
    WorkerExecutionToken, WorkerPoolError, WorkerPoolSnapshot, WorkerState, core::PoolRunQueue,
};

pub(super) struct BottomHalfPoolCore<const ENTRY_CAP: usize> {
    queue: PoolRunQueue<ENTRY_CAP>,
    slot: BottomHalfDrainSlot,
}

impl<const ENTRY_CAP: usize> BottomHalfPoolCore<ENTRY_CAP> {
    pub(super) const fn new() -> Self {
        Self {
            queue: PoolRunQueue::new(),
            slot: BottomHalfDrainSlot::new(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.queue.is_empty() && self.slot.state == WorkerState::Empty
    }

    pub(super) fn install_drain_slot(&mut self) -> bool {
        self.slot.install()
    }

    pub(super) fn runnable_len(&self) -> usize {
        self.queue.runnable_len()
    }

    pub(super) fn snapshot(&self) -> WorkerPoolSnapshot<1> {
        WorkerPoolSnapshot {
            installed_workers: usize::from(self.slot.state != WorkerState::Empty),
            nr_creating: 0,
            nr_idle: usize::from(self.slot.state == WorkerState::Idle),
            nr_preparing: usize::from(self.slot.state == WorkerState::Preparing),
            nr_claiming: usize::from(self.slot.state == WorkerState::Claiming),
            nr_running_state: usize::from(self.slot.state == WorkerState::Running),
            nr_sleeping: 0,
            nr_retire_requested: 0,
            nr_exiting: 0,
            nr_concurrency: usize::from(self.slot.state == WorkerState::Running),
            runnable: self.queue.runnable_len(),
            deferred: self.queue.deferred_len(),
            worker_states: [self.slot.state],
        }
    }

    pub(super) fn get_payload_mut(
        &mut self,
        owner: EntryOwner,
        key: EntryKey,
    ) -> Option<&mut EntryPayload> {
        self.queue.get_mut(owner, key)
    }

    pub(super) fn enqueue_runnable(
        &mut self,
        entry: PoolEntry,
        now: MonotonicInstant,
    ) -> Result<(), WorkerPoolError> {
        self.queue
            .push_runnable(entry)
            .map_err(WorkerPoolError::QueueFull)?;
        self.evaluate_dispatch(now);
        Ok(())
    }

    pub(super) fn enqueue_deferred(&mut self, entry: PoolEntry) -> Result<(), WorkerPoolError> {
        self.queue
            .push_deferred(entry)
            .map_err(WorkerPoolError::QueueFull)
    }

    pub(super) fn promote_one_deferred(
        &mut self,
        owner: EntryOwner,
        now: MonotonicInstant,
    ) -> Option<PoolEntry> {
        let entry = self.queue.promote_one_deferred(owner)?;
        self.evaluate_dispatch(now);
        Some(entry)
    }

    pub(super) fn remove_entry(
        &mut self,
        owner: EntryOwner,
        key: EntryKey,
    ) -> Option<(PoolEntry, QueueRemoveResult)> {
        self.queue.remove(owner, key)
    }

    pub(super) fn drain_ready_to_park(
        &mut self,
        _now: MonotonicInstant,
    ) -> Result<ParkDecision, WorkerPoolError> {
        match self.slot.state {
            WorkerState::Preparing if self.queue.runnable_len() != 0 => Ok(ParkDecision::Run),
            WorkerState::Preparing | WorkerState::Idle => {
                self.slot.state = WorkerState::Idle;
                Ok(ParkDecision::Wait)
            }
            WorkerState::Empty => Err(WorkerPoolError::InvalidWorker),
            _ => Err(WorkerPoolError::InvalidWorkerState),
        }
    }

    pub(super) fn prepare_runnable_candidate(
        &mut self,
        now: MonotonicInstant,
    ) -> Result<BottomHalfRunnableCandidateResult, WorkerPoolError> {
        if self.slot.state == WorkerState::Empty {
            return Err(WorkerPoolError::InvalidWorker);
        }
        if self.queue.runnable_len() == 0 {
            if matches!(self.slot.state, WorkerState::Preparing | WorkerState::Idle) {
                self.slot.state = WorkerState::Idle;
                return Ok(BottomHalfRunnableCandidateResult::Empty);
            }
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        if self.slot.state == WorkerState::Idle {
            self.evaluate_dispatch(now);
        }
        if self.slot.state != WorkerState::Preparing {
            return Err(WorkerPoolError::InvalidWorkerState);
        }

        match self.queue.pop_runnable() {
            Some(entry) => {
                let token = self.slot.reserve_execution_token();
                self.slot.state = WorkerState::Claiming;
                Ok(BottomHalfRunnableCandidateResult::Candidate(
                    BottomHalfRunnableCandidate { entry, token },
                ))
            }
            None => {
                self.slot.state = WorkerState::Idle;
                Ok(BottomHalfRunnableCandidateResult::Empty)
            }
        }
    }

    pub(super) fn commit_runnable_candidate(
        &mut self,
        token: WorkerExecutionToken,
        _now: MonotonicInstant,
    ) -> Result<(), WorkerPoolError> {
        if self.slot.state != WorkerState::Claiming || !self.slot.reserved_token_matches(token) {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.slot.start_execution(token);
        Ok(())
    }

    pub(super) fn discard_runnable_candidate(
        &mut self,
        token: WorkerExecutionToken,
        _now: MonotonicInstant,
    ) -> Result<BottomHalfRunnableCandidateDiscard, WorkerPoolError> {
        if self.slot.state != WorkerState::Claiming || !self.slot.reserved_token_matches(token) {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        if self.queue.runnable_len() != 0 {
            self.slot.state = WorkerState::Preparing;
            return Ok(BottomHalfRunnableCandidateDiscard { should_retry: true });
        }
        self.slot.state = WorkerState::Idle;
        Ok(BottomHalfRunnableCandidateDiscard {
            should_retry: false,
        })
    }

    pub(super) fn drain_finished(
        &mut self,
        token: WorkerExecutionToken,
        _now: MonotonicInstant,
    ) -> Result<(), WorkerPoolError> {
        if self.slot.state != WorkerState::Running || !self.slot.current_matches(token) {
            return Err(WorkerPoolError::InvalidWorkerState);
        }
        self.slot.clear_execution();
        if self.queue.runnable_len() != 0 {
            self.slot.state = WorkerState::Preparing;
        } else {
            self.slot.state = WorkerState::Idle;
        }
        Ok(())
    }

    fn evaluate_dispatch(&mut self, _now: MonotonicInstant) {
        if self.queue.runnable_len() != 0 && self.slot.state == WorkerState::Idle {
            self.slot.state = WorkerState::Preparing;
        }
    }
}
