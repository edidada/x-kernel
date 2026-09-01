// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Computes CRC32C checksums for ext4 metadata.
pub(crate) mod checksum;
/// Reads fixed-width little-endian values with slice bounds checks.
pub(crate) mod codec;
/// Decodes directory records and HTree index structures.
pub(crate) mod dir;
/// Defines the byte-level extent header, index, leaf, and tail formats.
pub(crate) mod extent;
/// Separates and negotiates ext4 compatible, incompatible, and read-only features.
pub(crate) mod features;
/// Decodes block-group descriptors and updates their checksummed counters.
mod group;
/// Decodes and encodes inode-table entries and their checksums.
pub(crate) mod inode;
/// Decodes the primary superblock and applies checked field updates.
pub(crate) mod superblock;
/// Defines the external extended-attribute block representation.
pub(crate) mod xattr;

pub use dir::DirectoryFileType;
pub use features::{CompatFeatures, FeatureSet, IncompatFeatures, ReadOnlyCompatFeatures};
pub use group::BlockGroupDescriptor;
pub(crate) use group::{
    GroupGeometry, GroupMutableState, decrement_group_free_blocks_count,
    decrement_group_free_inodes_count, decrement_group_used_directories_count,
    increment_group_free_blocks_count, increment_group_free_inodes_count,
    increment_group_used_directories_count, set_group_free_blocks_count,
    update_group_block_bitmap_metadata, update_group_inode_allocation_metadata,
    update_group_inode_bitmap_metadata,
};
pub use superblock::{Ext4DiskSuperblock, JournalFields};
