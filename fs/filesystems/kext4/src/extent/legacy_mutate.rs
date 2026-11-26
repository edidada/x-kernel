// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mutation admission and release for legacy inodes with direct pointers only.

use super::legacy::legacy_direct_block_pointers;
use crate::{
    BlockCount, CorruptKind, Ext4Error, Ext4Inode, Ext4Result, Ext4SbInfo, LogicalBlock,
    PhysicalBlock, UnsupportedKind, balloc::BlockReleaseKind, inode::InodeKind,
    jbd2::JournalHandle,
};

impl Ext4SbInfo<'_> {
    /// Keeps indirect trees read-only until their allocation and recovery
    /// contracts exist. Every direct pointer is checked before any mutation.
    pub(crate) fn ensure_legacy_direct_mutation_supported(
        &self,
        inode: &Ext4Inode,
    ) -> Ext4Result<()> {
        if inode.kind() != InodeKind::RegularFile
            || self.superblock().blocks_count() > u64::from(u32::MAX)
        {
            return Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentInode));
        }
        let pointers = legacy_direct_block_pointers(&inode.raw_i_block())?;
        for physical in pointers.into_iter().filter(|&physical| physical != 0) {
            if !self.is_inode_physical_block_valid(inode.number(), u64::from(physical), 1) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
            }
        }
        Ok(())
    }

    pub(crate) fn mapping_truncate_metadata_credits(
        &self,
        inode: &Ext4Inode,
        new_blocks: LogicalBlock,
    ) -> Ext4Result<u32> {
        if inode.has_extents() {
            return self.extent_truncate_metadata_credits(inode, new_blocks);
        }
        self.ensure_legacy_direct_mutation_supported(inode)?;
        let pointers = legacy_direct_block_pointers(&inode.raw_i_block())?;
        let released = pointers
            .iter()
            .enumerate()
            .filter(|&(logical, &physical)| logical as u64 >= new_blocks.get() && physical != 0)
            .count() as u32;
        // Inode, superblock and allocator admission headroom, then a bitmap
        // and descriptor for each potentially distinct data-block group.
        Ok(8 + released * 2)
    }

    pub(crate) fn has_initialized_inode_mapping_from(
        &self,
        inode: &Ext4Inode,
        logical: LogicalBlock,
    ) -> Ext4Result<bool> {
        if inode.has_extents() {
            return self.has_initialized_extent_mapping_from(inode, logical);
        }
        self.ensure_legacy_direct_mutation_supported(inode)?;
        Ok(legacy_direct_block_pointers(&inode.raw_i_block())?
            .iter()
            .enumerate()
            .any(|(slot, &physical)| slot as u64 >= logical.get() && physical != 0))
    }

    pub(crate) fn truncate_inode_mappings(
        &self,
        inode: &Ext4Inode,
        new_blocks: LogicalBlock,
        preserve_unwritten: bool,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        if inode.has_extents() {
            return self.truncate_extent_mappings(inode, new_blocks, preserve_unwritten, handle);
        }
        self.ensure_legacy_direct_mutation_supported(inode)?;
        let mut root = inode.raw_i_block();
        let pointers = legacy_direct_block_pointers(&root)?;
        let mut changed = false;
        for (logical, &physical) in pointers.iter().enumerate() {
            if logical as u64 >= new_blocks.get() && physical != 0 {
                root[logical * 4..logical * 4 + 4].fill(0);
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        self.update_inode_mapping_metadata(inode, inode.flags(), &root, None, handle)?;
        for (logical, physical) in pointers.into_iter().enumerate() {
            if logical as u64 >= new_blocks.get() && physical != 0 {
                self.release_allocated_block_range(
                    inode,
                    PhysicalBlock::new(u64::from(physical)),
                    BlockCount::new(1),
                    BlockReleaseKind::Data,
                    handle,
                )?;
            }
        }
        self.update_inode_blocks_metadata(inode, handle)
    }
}
