// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Journaled transitions between inline extents and direct block pointers.

use alloc::vec::Vec;

use super::{
    legacy::{LEGACY_DIRECT_BLOCKS, legacy_direct_block_pointers},
    validate::{decode_header, decode_leaf, entry_offset, validate_extent_entries},
};
use crate::{
    Ext4Error, Ext4Inode, Ext4Result, Ext4SbInfo, Ext4Timestamp, PhysicalBlock, UnsupportedKind,
    disk::{extent as disk_extent, inode as disk_inode},
    inode::InodeKind,
    jbd2::JournalCredits,
};

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../../ut-tests/extent/migrate_tests.rs");

impl Ext4SbInfo {
    /// Changes the mapping format without relocating payload blocks.
    /// KVFS has excluded buffered writers and faults, revoked mappings and
    /// drained writeback. The mount write guard excludes mapping walkers.
    /// Unsupported shapes are rejected before journal admission.
    pub(crate) fn migrate_inode_mapping(
        &mut self,
        inode: &Ext4Inode,
        flags: u32,
        timestamp: Ext4Timestamp,
    ) -> Ext4Result<()> {
        if self.is_quota_inode(inode.number()) || inode.is_noquota() {
            return Err(Ext4Error::Quota(kerrno::LinuxError::EPERM));
        }
        self.ensure_regular_file_mutation_supported(inode)?;
        if inode.kind() != InodeKind::RegularFile || !self.superblock().features().has_extents() {
            return Err(Ext4Error::Unsupported(
                UnsupportedKind::InodeMappingConversion,
            ));
        }
        let block = if inode.has_extents() {
            self.direct_root_from_extents(inode)?
        } else {
            self.extent_root_from_direct_blocks(inode)?
        };
        self.validate_inode_timestamp_update(inode, timestamp)?;
        self.initialize_inode_quotas(inode)?;
        let credits = self.quota_metadata_credits(JournalCredits::new(2));
        let journal = self.metadata_journal_for_allocation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            self.inode_extra_isize_allocation_blocks(inode),
        )?;
        let mut handle = journal.begin(credits)?;
        // Preallocations are not mappings. They must not survive a format
        // change and later be consumed by the other mapping implementation.
        self.release_preallocs(inode);
        let result =
            self.update_inode_mapping_metadata(inode, flags, &block, Some(timestamp), &mut handle);
        self.complete_metadata_mutation(handle, result)
    }

    fn direct_root_from_extents(
        &self,
        inode: &Ext4Inode,
    ) -> Ext4Result<[u8; disk_inode::INODE_BLOCK_BYTES]> {
        // Linux ext4_ind_migrate accepts an empty root or one inline extent
        // entirely within the twelve direct slots (fs/ext4/migrate.c).
        // Unlike Linux, our legacy writer cannot grow indirect mappings. Holes
        // still contribute to i_size even when the root has no allocated extent.
        let direct_bytes = LEGACY_DIRECT_BLOCKS * u64::from(self.layout().block_size());
        if self.superblock().blocks_count() > u64::from(u32::MAX) || inode.size() > direct_bytes {
            return Err(Ext4Error::Unsupported(
                UnsupportedKind::InodeMappingConversion,
            ));
        }
        let root = inode.raw_i_block();
        let header = decode_header(&root)?;
        if header.depth() != 0 || header.entries() > 1 {
            return Err(Ext4Error::Unsupported(
                UnsupportedKind::InodeMappingConversion,
            ));
        }
        validate_extent_entries(&root, header, None, None, |block, count| {
            self.is_inode_physical_block_valid(inode.number(), block, count)
        })?;
        let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
        if header.entries() == 0 {
            return Ok(block);
        }
        let extent = decode_leaf(&root, 0)?;
        let end = u64::from(extent.block()) + u64::from(extent.actual_len());
        if extent.is_unwritten() || end > LEGACY_DIRECT_BLOCKS {
            return Err(Ext4Error::Unsupported(
                UnsupportedKind::InodeMappingConversion,
            ));
        }
        for logical in u64::from(extent.block())..end {
            let physical = extent.start().get() + logical - u64::from(extent.block());
            let physical = u32::try_from(physical)
                .map_err(|_| Ext4Error::Unsupported(UnsupportedKind::InodeMappingConversion))?;
            let offset = logical as usize * 4;
            block[offset..offset + 4].copy_from_slice(&physical.to_le_bytes());
        }
        Ok(block)
    }

    fn extent_root_from_direct_blocks(
        &self,
        inode: &Ext4Inode,
    ) -> Ext4Result<[u8; disk_inode::INODE_BLOCK_BYTES]> {
        let pointers = legacy_direct_block_pointers(&inode.raw_i_block())?;
        let mut extents: Vec<(u32, u32, u32)> = Vec::new();
        for (logical, &physical) in pointers.iter().enumerate() {
            if physical == 0 {
                continue;
            }
            if !self.is_inode_physical_block_valid(inode.number(), u64::from(physical), 1) {
                return Err(Ext4Error::Corrupt(crate::CorruptKind::InvalidInode));
            }
            let logical = logical as u32;
            if let Some((start, first, count)) = extents.last_mut()
                && *start + *count == logical
                && u64::from(*first) + u64::from(*count) == u64::from(physical)
            {
                *count += 1;
            } else {
                extents.push((logical, physical, 1));
            }
        }
        let capacity = disk_extent::inline_extent_capacity()?;
        if extents.len() > usize::from(capacity) {
            return Err(Ext4Error::Unsupported(
                UnsupportedKind::InodeMappingConversion,
            ));
        }
        let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
        disk_extent::encode_header(&mut block, extents.len() as u16, capacity, 0, 0)?;
        for (index, (logical, physical, count)) in extents.into_iter().enumerate() {
            let offset = entry_offset(index)?;
            disk_extent::ExtentLeaf::new(
                logical,
                count as u16,
                PhysicalBlock::new(u64::from(physical)),
            )
            .encode(&mut block[offset..offset + disk_extent::EXTENT_ENTRY_SIZE])?;
        }
        Ok(block)
    }
}
