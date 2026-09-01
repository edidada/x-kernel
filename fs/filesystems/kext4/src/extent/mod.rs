// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Verifies and updates checksums on external extent-tree blocks.
mod checksum;
/// Traverses legacy direct and indirect block maps for checked reads.
mod legacy;
/// Resolves logical blocks and computes format-specific file-size limits.
///
/// Mapping validates every traversed extent or indirect block and returns a
/// contiguous mapped, unwritten, or sparse run without modifying metadata.
mod map;
/// Performs journaled extent insertion, conversion, removal, and tree rebalancing.
///
/// Mutations operate on extent-format inodes, validate the complete affected
/// path, and publish inode and external-node changes through the caller's
/// journal handle. Credit and depth bounds are checked before tree growth.
mod mutate;
/// Validates extent headers, ordering, bounds, and leaf mappings.
mod validate;

pub(crate) use mutate::ordered_writeback_credit_bound;

/// Covers extent lookup, validation, mutation, and size-limit behavior.
#[cfg(test)]
mod tests;

use crate::{BlockCount, PhysicalBlock};

bitflags::bitflags! {
    /// Semantics attached to a logical-block mapping result.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct BlockMappingFlags: u32 {
        /// The returned run was coalesced from adjacent mappings.
        const MERGED = 1 << 0;
        /// The hole is reserved for delayed allocation.
        const DELAYED = 1 << 1;
    }
}

/// Query-only logical block mapping result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockMapping {
    /// The logical range has no physical blocks and reads as zeroes.
    Hole {
        /// Number of contiguous logical blocks in the hole.
        len: BlockCount,
        /// Additional mapping properties, including delayed allocation.
        flags: BlockMappingFlags,
    },
    /// The logical range maps to initialized physical blocks.
    Mapped {
        /// First physical block in the contiguous mapping.
        physical: PhysicalBlock,
        /// Number of contiguous mapped blocks.
        len: BlockCount,
        /// Additional mapping properties.
        flags: BlockMappingFlags,
    },
    /// The logical range is preallocated but reads as zeroes.
    Unwritten {
        /// First physical block in the contiguous unwritten extent.
        physical: PhysicalBlock,
        /// Number of contiguous unwritten blocks.
        len: BlockCount,
        /// Additional mapping properties.
        flags: BlockMappingFlags,
    },
}

/// Target state for a newly inserted extent mapping.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentMappingState {
    /// The extent exposes initialized file data.
    Initialized,
    /// The extent is allocated but must read as zeroes until conversion.
    Unwritten,
}
