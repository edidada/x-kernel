// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bottom-half worker-pool state machine.

use kspin::SpinNoIrq;
use ktime_types::MonotonicInstant;

use super::{
    candidate::{BottomHalfRunnableCandidateDiscard, BottomHalfRunnableCandidateResult},
    core::BottomHalfPoolCore,
};
use crate::{
    EntryKey, EntryOwner, ParkDecision, PoolEntry, QueueRemoveResult, WorkerExecutionToken,
    WorkerPoolError, WorkerPoolPlacement, WorkerPoolSnapshot,
};

/// Worker-pool state driven by an external bottom-half execution context.
///
/// This type does not have a pool kind, ktask worker thread references, a
/// manager, or a dynamic worker table. It owns one drain slot and a bounded
/// runnable/deferred queue. The product layer is responsible for raising the
/// concrete bottom-half context when queue operations make work runnable.
pub struct BottomHalfWorkerPool<const ENTRY_CAP: usize> {
    inner: SpinNoIrq<BottomHalfPoolCore<ENTRY_CAP>>,
    placement: SpinNoIrq<WorkerPoolPlacement>,
}

impl<const ENTRY_CAP: usize> BottomHalfWorkerPool<ENTRY_CAP> {
    /// Creates one bottom-half pool instance.
    pub const fn new(placement: WorkerPoolPlacement) -> Self {
        Self {
            inner: SpinNoIrq::new(BottomHalfPoolCore::new()),
            placement: SpinNoIrq::new(placement),
        }
    }

    /// Returns where this bottom-half pool drains work.
    pub fn placement(&self) -> WorkerPoolPlacement {
        *self.placement.lock()
    }

    /// Reconfigures an empty const-initialized pool.
    pub fn configure_empty(&self, placement: WorkerPoolPlacement) -> bool {
        if !self.inner.lock().is_empty() {
            return false;
        }
        *self.placement.lock() = placement;
        true
    }

    /// Installs the externally driven bottom-half drain slot.
    pub fn install_drain_slot(&self) -> bool {
        self.inner.lock().install_drain_slot()
    }

    /// Returns current runnable FIFO length.
    pub fn runnable_len(&self) -> usize {
        self.inner.lock().runnable_len()
    }

    /// Returns a read-only state snapshot for diagnostics.
    pub fn snapshot(&self) -> WorkerPoolSnapshot<1> {
        self.inner.lock().snapshot()
    }

    /// Returns a read-only state snapshot without spinning.
    pub fn try_snapshot(&self) -> Option<WorkerPoolSnapshot<1>> {
        self.inner.try_lock().map(|inner| inner.snapshot())
    }

    /// Returns whether a queued entry with `owner` and `key` exists.
    pub fn has_entry(&self, owner: EntryOwner, key: EntryKey) -> bool {
        self.inner.lock().get_payload_mut(owner, key).is_some()
    }

    /// Enqueues an entry directly into the runnable FIFO.
    pub fn enqueue_runnable(
        &self,
        entry: PoolEntry,
        now: MonotonicInstant,
    ) -> Result<(), WorkerPoolError> {
        self.inner.lock().enqueue_runnable(entry, now)
    }

    /// Enqueues an entry into its owner's deferred lane.
    pub fn enqueue_deferred(&self, entry: PoolEntry) -> Result<(), WorkerPoolError> {
        self.inner.lock().enqueue_deferred(entry)
    }

    /// Promotes one deferred entry for `owner` into the runnable FIFO.
    pub fn promote_one_deferred(
        &self,
        owner: EntryOwner,
        now: MonotonicInstant,
    ) -> Option<PoolEntry> {
        self.inner.lock().promote_one_deferred(owner, now)
    }

    /// Removes an entry from runnable or deferred queues.
    pub fn remove_entry(
        &self,
        owner: EntryOwner,
        key: EntryKey,
    ) -> Option<(PoolEntry, QueueRemoveResult)> {
        self.inner.lock().remove_entry(owner, key)
    }

    /// Returns what the drain slot should do at the wait boundary.
    pub fn drain_ready_to_park(
        &self,
        now: MonotonicInstant,
    ) -> Result<ParkDecision, WorkerPoolError> {
        self.inner.lock().drain_ready_to_park(now)
    }

    /// Pops the oldest runnable entry and reserves it for external claim.
    pub fn prepare_runnable_candidate(
        &self,
        now: MonotonicInstant,
    ) -> Result<BottomHalfRunnableCandidateResult, WorkerPoolError> {
        self.inner.lock().prepare_runnable_candidate(now)
    }

    /// Marks a claimed runnable candidate as running.
    pub fn commit_runnable_candidate(
        &self,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<(), WorkerPoolError> {
        self.inner.lock().commit_runnable_candidate(token, now)
    }

    /// Discards a stale runnable candidate after external claim rejection.
    pub fn discard_runnable_candidate(
        &self,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<BottomHalfRunnableCandidateDiscard, WorkerPoolError> {
        self.inner.lock().discard_runnable_candidate(token, now)
    }

    /// Finishes one bottom-half drain execution.
    pub fn drain_finished(
        &self,
        token: WorkerExecutionToken,
        now: MonotonicInstant,
    ) -> Result<(), WorkerPoolError> {
        self.inner.lock().drain_finished(token, now)
    }
}
