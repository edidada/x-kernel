// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Age and log-space notifications for one mounted journal.

use alloc::sync::Arc;

use kalloc_types::KBox;
use ktime_types::TimeSpan;
use kvfs::SuperBlock;
use kworkng::{ArmError, DelayedWorkHandle, SubmitError};

use super::{Ext4Error, Ext4Result, Ext4SbInfo, MountedJournal, RecoveryFlagPolicy, lock};
use crate::{jbd2::TransactionId, vfs::Ext4Mount};

pub(crate) const JOURNAL_COMMIT_INTERVAL: TimeSpan = TimeSpan::from_secs(5);

/// Clones refer to the same one-shot work item. The callback only retains a
/// weak superblock reference and obtains mount exclusion before inspecting it.
#[derive(Clone)]
pub(crate) struct JournalWorker {
    work: DelayedWorkHandle<KBox<dyn Fn() + Send + Sync>>,
    pub(super) commit_interval: TimeSpan,
}

enum BackgroundWork {
    Checkpoint,
    Commit(TransactionId),
    Wait(TimeSpan),
    Idle,
}

impl JournalWorker {
    pub(super) fn notify_after(&self, delay: TimeSpan) -> Ext4Result<()> {
        // Age notifications preserve an earlier wakeup. Space pressure promotes
        // a pending timer to ready work. Neither operation executes inline.
        let queued = if delay.is_zero() {
            self.work.modify_default(delay, None)
        } else {
            self.work.queue_default(delay, None)
        };
        match queued {
            Ok(_) | Err(ArmError::Admission(SubmitError::AlreadyQueued)) => Ok(()),
            Err(_) => Err(Ext4Error::Device(block::DriverError::BadState)),
        }
    }

    /// Wait in sleepable teardown context after detaching under mount exclusion.
    pub(crate) fn shutdown(self) -> Ext4Result<()> {
        self.work
            .shutdown()
            .map_err(|_| Ext4Error::Device(block::DriverError::BadState))
    }
}

impl MountedJournal {
    pub(super) fn worker(&self) -> Option<JournalWorker> {
        lock(&self.state).worker.clone()
    }

    pub(crate) fn detach_worker(&self) -> Option<JournalWorker> {
        lock(&self.state).worker.take()
    }

    /// Mount exclusion keeps the queue frontier and the running identity stable
    /// across these snapshots. No short journal lock spans notification or I/O.
    fn background_work(&self, worker: &JournalWorker) -> Ext4Result<BackgroundWork> {
        if self.needs_background_checkpoint()? {
            return Ok(BackgroundWork::Checkpoint);
        }
        let Some((transaction, start)) = self.transactions.running_since()? else {
            return Ok(BackgroundWork::Idle);
        };
        let (_, now) = ktime::realtime_and_monotonic();
        let delay = worker
            .commit_interval
            .saturating_sub(now.saturating_duration_since(start));
        Ok(if delay.is_zero() {
            BackgroundWork::Commit(transaction)
        } else {
            BackgroundWork::Wait(delay)
        })
    }

    fn needs_background_checkpoint(&self) -> Ext4Result<bool> {
        match self.newest_persisted_commit()? {
            Some(commit) => {
                commit.needs_background_checkpoint(&lock(&self.state).storage.superblock)
            }
            None => Ok(false),
        }
    }

    pub(super) fn notify_space_pressure(&self) -> Ext4Result<()> {
        if let Some(worker) = self.worker()
            && self.needs_background_checkpoint()?
        {
            worker.notify_after(TimeSpan::ZERO)?;
        }
        Ok(())
    }

    /// Called once under mount write exclusion at the end of initialization.
    /// Recovery or mount setup may already have left a running transaction.
    pub(crate) fn start_worker(
        &self,
        sb: &Arc<SuperBlock>,
        commit_interval: TimeSpan,
    ) -> Ext4Result<()> {
        let weak = Arc::downgrade(sb);
        let callback: KBox<dyn Fn() + Send + Sync> = KBox::new_nowait(move || {
            if let Some(sb) = weak.upgrade()
                && let Ok(mount) = sb.private::<Ext4Mount>()
            {
                let mut state = mount.write(&sb);
                if let Err(error) = state.progress_background_journal() {
                    error!("KExt4 background journal failed: {error:?}");
                }
            }
        })?;
        let worker = JournalWorker {
            work: DelayedWorkHandle::new(callback)
                .map_err(|_| Ext4Error::Device(block::DriverError::BadState))?,
            commit_interval,
        };
        let delay = match self.background_work(&worker)? {
            BackgroundWork::Commit(_) | BackgroundWork::Checkpoint => Some(TimeSpan::ZERO),
            BackgroundWork::Wait(delay) => Some(delay),
            BackgroundWork::Idle => None,
        };
        if let Some(delay) = delay {
            worker.notify_after(delay)?;
        }
        // A ready callback waits for our mount write guard before reading this slot.
        lock(&self.state).worker = Some(worker);
        Ok(())
    }
}

impl Ext4SbInfo {
    /// Runs in a normal worker task under mount write exclusion. Producers
    /// reaching the same mount after the final check may enqueue a successor
    /// even while this occurrence is returning to the queue.
    fn progress_background_journal(&mut self) -> Ext4Result<()> {
        let Some(journal) = self.journal.clone() else {
            return Ok(());
        };
        let Some(worker) = journal.worker() else {
            return Ok(());
        };
        if journal.is_aborted() {
            return Ok(());
        }
        let result = (|| loop {
            match journal.background_work(&worker)? {
                BackgroundWork::Checkpoint => self.drain_pending_checkpoints()?,
                BackgroundWork::Commit(transaction) => self.commit_metadata_transaction_inner(
                    &journal.lock_persistence(),
                    transaction,
                    RecoveryFlagPolicy::ClearAfterCheckpoint,
                )?,
                BackgroundWork::Wait(delay) => return worker.notify_after(delay),
                BackgroundWork::Idle => return Ok(()),
            }
        })();
        result.map_err(|error| self.abort_metadata_journal(error))
    }
}
