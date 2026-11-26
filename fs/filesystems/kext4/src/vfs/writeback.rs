// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned data submission and ordered metadata completion for buffered writeback.

use alloc::{sync::Arc, vec::Vec};

use ksync::Mutex;
use kvfs::{
    AddressSpace, SuperBlock, VfsError, VfsInode, VfsResult, WritebackControl, WritebackSyncMode,
};
use pagecache::{OwnedWritebackCompletion, WritebackBatch};

use super::{MAX_WRITEBACK_BYTES, ext4_private, into_vfs_err};
use crate::{
    Ext4Inode, Ext4SyncIntent, Ext4WritebackResult, file::PendingMappedWriteback,
    writeback_io::MAX_INFLIGHT_BATCHES,
};

enum CompletionKind {
    Mapped,
    Ordered,
    Direct(crate::file::DirectWritePins),
}

struct PendingMappedBatch {
    // Drain the device before PageCache can release or redirty its snapshot.
    io: PendingMappedWriteback,
    context: CompletionContext,
}

struct CompletionContext {
    // Retained until queue removal and wakeup, so final inode eviction cannot
    // run while its own completion is still registered as in-flight.
    inode: Arc<VfsInode>,
    private: Arc<Ext4Inode>,
    super_block: Arc<SuperBlock>,
    batch: Mutex<Option<(OwnedWritebackCompletion<AddressSpace>, CompletionKind)>>,
    offset: u64,
}

impl CompletionContext {
    fn finish(&self, data: Ext4WritebackResult) -> VfsResult<()> {
        let Some((completion, kind)) = self.batch.lock().take() else {
            return Ok(());
        };
        let result = self.finish_metadata(data, kind);
        if let Err(failure) = result {
            self.record_error(into_vfs_err(failure.error()));
        }
        let completed = finish_cache_batch(&self.private, self.offset, completion, result);
        if result.is_ok()
            && let Err(error) = completed
        {
            self.record_error(error);
        }
        completed
    }

    fn record_error(&self, error: VfsError) {
        self.inode.writeback_errors().record(error);
        self.super_block.writeback_errors().record(error);
    }

    fn finish_metadata(
        &self,
        data: Ext4WritebackResult,
        kind: CompletionKind,
    ) -> Ext4WritebackResult {
        let ext4 = ext4_private(&self.super_block)
            .map_err(|_| crate::Ext4Error::Device(block::DriverError::BadState))?;
        if matches!(kind, CompletionKind::Mapped)
            && !self.private.has_delalloc_reservations()
            && (self.inode.write_count() != 0 || self.private.prealloc_map().is_empty())
        {
            self.private.note_data_completion();
            return data;
        }
        let _metadata = self.private.writeback_io().lock_metadata();
        let mut state = ext4.writeback(&self.super_block);
        match kind {
            CompletionKind::Mapped => state.finish_mapped_writeback(
                &self.private,
                self.offset,
                data,
                Ext4SyncIntent::DataOnly,
                || self.inode.write_count() != 0,
            ),
            CompletionKind::Direct(pins) => {
                self.private.note_data_completion();
                state.finish_direct_writeback(&self.private, self.offset, data, &pins, None)
            }
            CompletionKind::Ordered => state.finish_prepared_writeback(
                &self.private,
                self.offset,
                data,
                Ext4SyncIntent::DataOnly,
                || self.inode.write_count() != 0,
            ),
        }
    }
}

fn finish_cache_batch(
    inode: &Ext4Inode,
    offset: u64,
    completion: OwnedWritebackCompletion<AddressSpace>,
    result: Ext4WritebackResult,
) -> VfsResult<()> {
    let (prefix, error) = match result {
        Ok(bytes) => (bytes, None),
        Err(failure) => {
            error!(
                "KExt4 inode {} writeback at offset {offset} failed after {} bytes: {:?}",
                inode.number().get(),
                failure.completed_bytes(),
                failure.error()
            );
            (
                failure.completed_bytes(),
                Some(into_vfs_err(failure.error())),
            )
        }
    };
    let settled = completion.complete(prefix);
    error.map_or_else(|| settled.map(|_| ()), Err)
}

struct WritebackPass {
    inode: Arc<VfsInode>,
    private: Arc<Ext4Inode>,
    super_block: Arc<SuperBlock>,
    pending_mapped: Vec<PendingMappedBatch>,
}

impl WritebackPass {
    fn submit(
        &mut self,
        mapping: &AddressSpace,
        control: &mut WritebackControl,
        batch: WritebackBatch<'_>,
    ) -> VfsResult<()> {
        let queue = self.private.writeback_io().clone();
        // Neighboring folios can share a physical block. Prepare their partial
        // reads only after the previous read/modify/write has settled.
        let shares_block = self.super_block.block_size() > super::PAGE_SIZE_4K as u64;
        if !queue.has_capacity(shares_block) {
            if control.sync_mode() == WritebackSyncMode::None {
                control.account_skipped(batch.page_count());
                return Err(VfsError::WouldBlock);
            }
            queue.wait_capacity(shares_block);
        }
        let offset = batch.offset();
        let super_block = self.super_block.clone();
        let ext4 = ext4_private(&super_block)?;
        let plan = ext4
            .read()
            .prepare_mapped_writeback(
                &self.private,
                offset,
                batch.data(),
                self.inode.size(),
                Ext4SyncIntent::DataOnly,
            )
            .map_err(into_vfs_err)
            .inspect_err(|&error| self.record_error(error))?;
        let (data, completion) = batch.into_parts();
        let completion = mapping.retain_writeback_completion(completion)?;
        let (plan, kind) = match plan {
            Some(plan) if control.sync_mode() == WritebackSyncMode::All && !shares_block => {
                // Earlier allocating batches may still own metadata publication.
                // The submission guard excludes mapping removal until this local
                // pipeline drains, so no worker is needed for these overwrites.
                queue.drain();
                let io = plan
                    .submit(data)
                    .map_err(into_vfs_err)
                    .inspect_err(|&error| self.record_error(error))?;
                let context = self.completion(offset, completion, CompletionKind::Mapped);
                self.pending_mapped.push(PendingMappedBatch { io, context });
                if self.pending_mapped.len() == MAX_INFLIGHT_BATCHES {
                    self.drain_mapped()?;
                }
                return Ok(());
            }
            Some(plan) => (plan.into_async(), CompletionKind::Mapped),
            None => {
                self.drain_mapped()?;
                let _metadata = queue.lock_metadata();
                let mut state = ext4.writeback(&self.super_block);
                if self.private.has_extents() {
                    state
                        .prepare_allocating_writeback(
                            &self.private,
                            offset,
                            data.len(),
                            self.inode.size(),
                            Ext4SyncIntent::DataOnly,
                        )
                        .map(|plan| (plan, CompletionKind::Ordered))
                } else {
                    state
                        .prepare_direct_writeback(
                            &self.private,
                            offset,
                            data.len(),
                            self.inode.size(),
                        )
                        .map(|(plan, pins)| (plan, CompletionKind::Direct(pins)))
                }
                .map_err(into_vfs_err)
                .inspect_err(|&error| self.record_error(error))?
            }
        };
        let context = self.completion(offset, completion, kind);
        plan.submit(
            data,
            &queue,
            move |result| {
                let _ = context.finish(result);
            },
            |error| self.record_error(into_vfs_err(error)),
        )
        .map_err(into_vfs_err)
    }

    fn completion(
        &self,
        offset: u64,
        completion: OwnedWritebackCompletion<AddressSpace>,
        kind: CompletionKind,
    ) -> CompletionContext {
        CompletionContext {
            inode: self.inode.clone(),
            private: self.private.clone(),
            super_block: self.super_block.clone(),
            batch: Mutex::new(Some((completion, kind))),
            offset,
        }
    }

    fn record_error(&self, error: VfsError) {
        self.inode.writeback_errors().record(error);
        self.super_block.writeback_errors().record(error);
    }

    fn drain_mapped(&mut self) -> VfsResult<()> {
        let mut first_error = None;
        for pending in self.pending_mapped.drain(..) {
            let result = pending.io.wait();
            if let Err(error) = pending.context.finish(result) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

pub(super) fn writepages(mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
    let inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
    let private = inode.downcast::<Ext4Inode>()?;
    let errors = mapping.writeback_errors();
    // File cursors report historical failures. Include new failures while
    // waiting for submission or old I/O without inheriting unseen history.
    let error_generation = errors.current_generation();
    let guard = private.lock_writeback_submission();
    let queue = private.writeback_io();
    let mut pass = WritebackPass {
        super_block: inode.super_block()?,
        inode: inode.clone(),
        private: private.clone(),
        pending_mapped: Vec::new(),
    };
    let submitted = mapping.submit_cached_ranges(control, MAX_WRITEBACK_BYTES, |control, batch| {
        pass.submit(mapping, control, batch)
    });
    // Always settle already submitted BIOs, including after a later admission error.
    let mapped_completed = pass.drain_mapped();
    // Generic ALL preparation waits/revalidates selected busy folios. This
    // finite prefix also settles old clean or detached owners, so no entry-wide
    // drain is needed before selecting and submitting new work.
    let target = queue.snapshot();
    drop(guard);
    if control.sync_mode() == WritebackSyncMode::All {
        queue.wait_through(target);
        let released = queue.flush_completions().map_err(into_vfs_err);
        let completed = errors.check(error_generation);
        return submitted.and(mapped_completed).and(released).and(completed);
    }
    submitted.and(mapped_completed)
}
