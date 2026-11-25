// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ordered synchronous writeback for the twelve legacy direct block slots.

use alloc::sync::Arc;

use super::{AllocatedWriteRun, OrderedWritebackData, PartialBlockWrite, RegularWriteMetadata};
use crate::{
    BlockCount, Ext4Error, Ext4Inode, Ext4Result, Ext4SbInfo, Ext4SyncIntent, FilesystemBlock,
    LogicalBlock, UnsupportedKind,
    extent::{LEGACY_DIRECT_BLOCKS, legacy_direct_block_pointers},
    jbd2::JournalCredits,
    mballoc::{Ext4AllocationFlags, Ext4AllocationRequest},
};

impl Ext4SbInfo {
    pub(super) fn writeback_legacy_direct_at(
        &mut self,
        inode: &Arc<Ext4Inode>,
        offset: u64,
        input: &[u8],
        visible_size: u64,
        intent: Ext4SyncIntent,
    ) -> Ext4Result<usize> {
        self.ensure_legacy_direct_mutation_supported(inode)?;
        let block_size = u64::from(self.layout().block_size());
        let end = offset
            .checked_add(input.len() as u64)
            .ok_or(Ext4Error::Overflow)?;
        let first = offset / block_size;
        let last = end.div_ceil(block_size);
        if last > LEGACY_DIRECT_BLOCKS {
            return Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentInode));
        }
        let mut root = inode.raw_i_block();
        let pointers = legacy_direct_block_pointers(&root)?;
        let allocations = pointers[first as usize..last as usize]
            .iter()
            .filter(|&&physical| physical == 0)
            .count() as u32;
        self.initialize_inode_quotas(inode)?;
        let credits = self.quota_metadata_credits(JournalCredits::new(8 + allocations * 3));
        let journal = self.metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            u64::from(allocations) + self.inode_extra_isize_allocation_blocks(inode),
        )?;
        let mut handle = journal.begin(credits)?;
        let result = (|| {
            for logical in first..last {
                let old = pointers[logical as usize];
                let physical = if old == 0 {
                    let request = Ext4AllocationRequest::new(
                        inode,
                        LogicalBlock::new(logical),
                        None,
                        BlockCount::new(1),
                        BlockCount::new(1),
                        Ext4AllocationFlags::EXACT,
                    )?;
                    let (allocation, _) = self.allocate_blocks_for_write_with_prealloc(
                        request,
                        inode,
                        0,
                        &mut handle,
                    )?;
                    let physical = u32::try_from(allocation.physical_start().get())
                        .map_err(|_| Ext4Error::Overflow)?;
                    root[logical as usize * 4..logical as usize * 4 + 4]
                        .copy_from_slice(&physical.to_le_bytes());
                    physical
                } else {
                    old
                };
                let start = offset.max(logical * block_size);
                let finish = end.min((logical + 1) * block_size);
                let run = AllocatedWriteRun {
                    first_block: FilesystemBlock::new(u64::from(physical)),
                    block_count: 1,
                    input_offset: (start - offset) as usize,
                    in_block: (start % block_size) as usize,
                    write_len: (finish - start) as usize,
                };
                self.write_ordered_run(
                    &run,
                    OrderedWritebackData::Write(input),
                    block_size as usize,
                    if old == 0 {
                        PartialBlockWrite::ZeroFill
                    } else {
                        PartialBlockWrite::PreserveExisting
                    },
                )?;
                handle.require_ordered_data_flush()?;
            }
            // The same transaction makes the initialized payload reachable and
            // records its block accounting. It cannot commit before data flush.
            self.update_inode_mapping_metadata(inode, inode.flags(), &root, None, &mut handle)?;
            let disk_size = if intent.is_data_only() {
                inode.disk_size().max(end)
            } else {
                visible_size
            };
            self.update_regular_inode_write_metadata(
                inode,
                disk_size,
                RegularWriteMetadata::SizeOnly,
                &mut handle,
            )
        })();
        self.complete_metadata_mutation(handle, result)?;
        self.release_delalloc_range(inode, LogicalBlock::new(first), last - first)?;
        Ok(input.len())
    }
}
