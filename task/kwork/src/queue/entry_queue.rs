// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kworkerpool::{PoolEntry, PoolRunQueue, QueueRemoveResult};

use super::WorkEntry;
use crate::{BarrierAttachResult, ScheduledWork, WorkBarrier};
#[cfg(unittest)]
use crate::{QueueOwner, WorkColor, WorkInstanceId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingBarrierAttach {
    Attached,
    Missing,
    Full,
}

/// Maximum number of pending work entries one worker pool's storage holds.
///
/// Runnable and inactive entries share this bounded capacity so enqueue remains
/// IRQ-safe and never allocates. Logical workqueue policy decides whether a new
/// entry is immediately runnable or deferred; the storage only performs
/// mechanical lane moves.
pub const MAX_WORKQUEUE_PENDING: usize = kbuild_config::WORKQUEUE_PENDING_CAP;

/// Pending lane from which an entry was removed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkEntryLane {
    /// Entry came from the pool runnable list and participates in active count.
    Runnable,
    /// Entry came from the binding inactive list and is throttled by max_active.
    Inactive,
}

/// Entry removed from one of the pending storage lanes.
pub(crate) struct PendingWorkEntry {
    entry: WorkEntry,
    lane: WorkEntryLane,
}

/// Linux-like pending storage for one worker pool.
///
/// The concrete bounded queue substrate is owned by `kworkerpool`; `kwork`
/// supplies the owner key (`binding_key`) and entry key (`ScheduledWork::key`),
/// while keeping workqueue policy, barriers, and flush semantics here.
pub(crate) struct PendingWorkStore {
    entries: PoolRunQueue<usize, usize, WorkEntry, MAX_WORKQUEUE_PENDING>,
}

impl PendingWorkEntry {
    fn new(entry: WorkEntry, lane: WorkEntryLane) -> Self {
        Self { entry, lane }
    }

    pub(crate) fn entry(&self) -> &WorkEntry {
        &self.entry
    }

    pub(crate) fn entry_mut(&mut self) -> &mut WorkEntry {
        &mut self.entry
    }

    pub(crate) fn is_runnable(&self) -> bool {
        self.lane == WorkEntryLane::Runnable
    }

    pub(crate) fn into_work(self) -> ScheduledWork {
        self.entry.into_work()
    }
}

impl PendingWorkStore {
    pub(crate) const fn new() -> Self {
        Self {
            entries: PoolRunQueue::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn push_entry(&mut self, entry: WorkEntry, lane: WorkEntryLane) -> Result<(), ()> {
        let owner = entry.binding_key();
        let key = entry.work().key();
        let entry = PoolEntry::new(owner, key, entry);
        match lane {
            WorkEntryLane::Runnable => self.entries.push_runnable(entry),
            WorkEntryLane::Inactive => self.entries.push_deferred(entry),
        }
        .map_err(|_| ())
    }

    pub(crate) fn push_runnable(&mut self, entry: WorkEntry) -> Result<(), ()> {
        self.push_entry(entry, WorkEntryLane::Runnable)
    }

    #[cfg(unittest)]
    pub(crate) fn push(
        &mut self,
        work: &ScheduledWork,
        owner: QueueOwner,
        color: WorkColor,
    ) -> Result<(), ()> {
        self.push_runnable(WorkEntry::new(
            work.clone(),
            owner,
            color,
            WorkInstanceId::for_tests(1),
        ))
    }

    pub(crate) fn push_inactive(&mut self, entry: WorkEntry) -> Result<(), ()> {
        self.push_entry(entry, WorkEntryLane::Inactive)
    }

    #[cfg(unittest)]
    pub(crate) fn pending_len_for_binding(&self, binding_key: usize) -> usize {
        self.entries.runnable_len_for_owner(&binding_key)
            + self.entries.deferred_len_for_owner(&binding_key)
    }

    #[cfg(unittest)]
    pub(crate) fn runnable_len_for_binding(&self, binding_key: usize) -> usize {
        self.entries.runnable_len_for_owner(&binding_key)
    }

    pub(crate) fn deactivate_runnable_for_binding(&mut self, binding_key: usize) -> usize {
        self.entries.defer_runnable_for_owner(&binding_key)
    }

    pub(crate) fn activate_first_inactive_for_binding(&mut self, binding_key: usize) -> bool {
        self.entries.promote_deferred(&binding_key, 1) != 0
    }

    pub(crate) fn remove_work_for_key(
        &mut self,
        work: &ScheduledWork,
        binding_key: usize,
    ) -> Option<PendingWorkEntry> {
        self.entries
            .remove(&binding_key, &work.key())
            .map(|(entry, lane)| {
                PendingWorkEntry::new(
                    entry.into_payload(),
                    match lane {
                        QueueRemoveResult::Runnable => WorkEntryLane::Runnable,
                        QueueRemoveResult::Deferred => WorkEntryLane::Inactive,
                    },
                )
            })
    }

    pub(crate) fn attach_barrier_to_work_for_key(
        &mut self,
        work: &ScheduledWork,
        binding_key: usize,
        barrier: WorkBarrier,
    ) -> PendingBarrierAttach {
        let Some(entry) = self.entries.get_mut(&binding_key, &work.key()) else {
            return PendingBarrierAttach::Missing;
        };
        match entry.attach_barrier(barrier) {
            BarrierAttachResult::Attached => PendingBarrierAttach::Attached,
            BarrierAttachResult::Full => PendingBarrierAttach::Full,
        }
    }

    pub(crate) fn pop_runnable_candidate(&mut self) -> Option<WorkEntry> {
        self.entries.pop_runnable().map(PoolEntry::into_payload)
    }
}
