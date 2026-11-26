// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Data preparation and ordered publication for the twelve legacy direct block slots.

use alloc::{sync::Arc, vec::Vec};

use super::{
    AllocatedWriteRun, OrderedWritebackData, PartialBlockWrite, RegularWriteMetadata,
    async_writeback::{PreparedRun, PreparedWriteback},
};
use crate::{
    Ext4Error, Ext4Inode, Ext4Result, Ext4SbInfo, Ext4SyncIntent, Ext4WritebackResult,
    FilesystemBlock, LogicalBlock, UnsupportedKind,
    extent::{LEGACY_DIRECT_BLOCKS, legacy_direct_block_pointers},
    jbd2::JournalCredits,
    superblock::Ext4Storage,
};

impl Ext4SbInfo<'_> {
    pub(super) fn writeback_legacy_direct_at(
        &mut self,
        inode: &Arc<Ext4Inode>,
        offset: u64,
        input: &[u8],
        visible_size: u64,
        intent: Ext4SyncIntent,
    ) -> Ext4WritebackResult {
        let (plan, pins) =
            self.prepare_direct_writeback(inode, offset, input.len(), visible_size)?;
        let data: Ext4WritebackResult = self.with_journal_io(|storage, _| {
            let mut bytes = 0;
            for run in &plan.runs {
                storage
                    .write_ordered_run(
                        &run.mapping,
                        OrderedWritebackData::Write(input),
                        storage.device.block_size(),
                        run.partial,
                    )
                    .map_err(|error| crate::Ext4WritebackError::new(bytes, error))?;
                bytes += run.mapping.write_len;
            }
            Ok(bytes)
        });
        if intent.requires_full_metadata()
            && let Err(failure) = data
        {
            return Err(self.fail_metadata_mutation(false, failure.error()).into());
        }
        self.finish_direct_writeback(
            inode,
            offset,
            data,
            &pins,
            intent.requires_full_metadata().then_some(visible_size),
        )
    }
}

/// The allocator excludes pinned physical blocks from allocation and speculative
/// preallocation until all BIOs have retired. On disk the holes stay free.
pub(crate) struct DirectWritePins {
    storage: Arc<Ext4Storage>,
    blocks: Vec<(LogicalBlock, FilesystemBlock)>,
}

impl Drop for DirectWritePins {
    fn drop(&mut self) {
        let mut alloc = self.storage.allocator.lock();
        for &(_, block) in &self.blocks {
            alloc.direct_write_pins.remove(&block);
        }
    }
}

impl Ext4SbInfo<'_> {
    /// Caller holds inode submission through queue installation. Mapping removal
    /// drains that queue before freeing blocks or replacing the direct root.
    pub(crate) fn prepare_direct_writeback(
        &mut self,
        inode: &Ext4Inode,
        offset: u64,
        total: usize,
        visible_size: u64,
    ) -> Ext4Result<(PreparedWriteback, DirectWritePins)> {
        self.ensure_regular_file_mutation_supported(inode)?;
        let block_size = self.device.block_size() as u64;
        let end = offset
            .checked_add(total as u64)
            .ok_or(Ext4Error::Overflow)?;
        if total == 0 || end > visible_size || visible_size < inode.disk_size() {
            return Err(Ext4Error::OutOfBounds);
        }
        let first = offset / block_size;
        let last = end.div_ceil(block_size);
        if last > LEGACY_DIRECT_BLOCKS {
            return Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentInode));
        }
        let pointers = legacy_direct_block_pointers(&inode.raw_i_block())?;
        let missing = pointers[first as usize..last as usize]
            .iter()
            .filter(|&&p| p == 0)
            .count() as u32;
        self.initialize_inode_quotas(inode)?;
        // Reject an unrepresentable transaction and recover pending frees
        // before reserving physical targets or submitting any data.
        let credits = self.quota_metadata_credits(JournalCredits::new(8 + 3 * missing));
        let journal = self.metadata_journal()?;
        if !journal.can_fit_handle(credits) {
            return Err(Ext4Error::NoSpace);
        }
        if missing != 0 && self.has_pending_block_frees()? {
            self.metadata_journal_for_allocation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
                u64::from(missing) + self.inode_extra_isize_allocation_blocks(inode),
            )?;
        }
        let mut pins = DirectWritePins {
            storage: self.storage.clone(),
            blocks: Vec::with_capacity(missing as usize),
        };
        let mut runs = Vec::with_capacity((last - first) as usize);
        for logical in first..last {
            let pointer = pointers[logical as usize];
            let (physical, partial) = if pointer == 0 {
                let block = self.pin_direct_write_block(inode)?;
                pins.blocks.push((LogicalBlock::new(logical), block));
                (block, PartialBlockWrite::ZeroFill)
            } else {
                (
                    FilesystemBlock::new(u64::from(pointer)),
                    PartialBlockWrite::PreserveExisting,
                )
            };
            let start = offset.max(logical * block_size);
            let finish = end.min((logical + 1) * block_size);
            runs.push(PreparedRun {
                mapping: AllocatedWriteRun {
                    first_block: physical,
                    block_count: 1,
                    input_offset: (start - offset) as usize,
                    in_block: (start % block_size) as usize,
                    write_len: (finish - start) as usize,
                },
                partial,
            });
        }
        Ok((
            PreparedWriteback {
                device: self.device.clone(),
                runs,
            },
            pins,
        ))
    }

    /// Publish only the contiguous successful prefix. The same ordered
    /// transaction claims its blocks, installs direct pointers and updates size.
    pub(crate) fn finish_direct_writeback(
        &mut self,
        inode: &Ext4Inode,
        offset: u64,
        data: Ext4WritebackResult,
        pins: &DirectWritePins,
        full_size: Option<u64>,
    ) -> Ext4WritebackResult {
        let bytes = data.unwrap_or_else(|failure| failure.completed_bytes());
        if bytes == 0 {
            return data;
        }
        let published = self.publish_direct_prefix(inode, offset, bytes, pins, full_size);
        match (published, data) {
            (Ok(()), result) => result,
            (Err(metadata), Err(io)) => {
                error!(
                    "KExt4 inode {} failed to publish direct write at {offset}: {metadata:?}",
                    inode.number().get()
                );
                Err(crate::Ext4WritebackError::new(0, io.error()))
            }
            (Err(error), Ok(_)) => Err(error.into()),
        }
    }

    fn publish_direct_prefix(
        &mut self,
        inode: &Ext4Inode,
        offset: u64,
        bytes: usize,
        pins: &DirectWritePins,
        full_size: Option<u64>,
    ) -> Ext4Result<()> {
        let end = offset
            .checked_add(bytes as u64)
            .ok_or(Ext4Error::Overflow)?;
        let block_size = self.device.block_size() as u64;
        let first = offset / block_size;
        let last = end.div_ceil(block_size);
        let completed = &pins.blocks[..pins
            .blocks
            .partition_point(|(logical, _)| logical.get() < last)];
        let credits =
            self.quota_metadata_credits(JournalCredits::new(8 + completed.len() as u32 * 3));
        let journal = self.metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            self.inode_extra_isize_allocation_blocks(inode),
        )?;
        let mut handle = journal.begin(credits)?;
        let result = (|| {
            handle.require_ordered_data_flush()?;
            let mut root = inode.raw_i_block();
            for &(logical, physical) in completed {
                let slot = logical.get() as usize * 4;
                if root[slot..slot + 4] != [0; 4] {
                    return Err(Ext4Error::InvalidJournalTransaction);
                }
                self.claim_direct_write(inode, logical, physical, &mut handle)?;
                let pointer = u32::try_from(physical.get()).map_err(|_| Ext4Error::Overflow)?;
                root[slot..slot + 4].copy_from_slice(&pointer.to_le_bytes());
            }
            if !completed.is_empty() {
                self.update_inode_mapping_metadata(inode, inode.flags(), &root, None, &mut handle)?;
            }
            self.update_regular_inode_write_metadata(
                inode,
                full_size.unwrap_or_else(|| inode.disk_size().max(end)),
                RegularWriteMetadata::SizeOnly,
                &mut handle,
            )
        })();
        self.complete_metadata_mutation(handle, result)?;
        self.release_delalloc_range(inode, LogicalBlock::new(first), last - first)
    }
}
