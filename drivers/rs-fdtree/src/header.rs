// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! FdtHeader

use crate::parsing::{BigEndianU32, FdtData};

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub(crate) struct FdtHeader {
    /// FDT header magic
    magic: BigEndianU32,
    /// Total size in bytes of the FDT structure
    pub(crate) totalsize: BigEndianU32,
    /// Offset in bytes from the start of the header to the structure block
    off_dt_struct: BigEndianU32,
    /// Offset in bytes from the start of the header to the strings block
    off_dt_strings: BigEndianU32,
    /// Offset in bytes from the start of the header to the memory reservation
    /// block
    pub(crate) off_mem_rsvmap: BigEndianU32,
    /// FDT version
    version: BigEndianU32,
    /// Last compatible FDT version
    last_comp_version: BigEndianU32,
    /// System boot CPU ID
    boot_cpuid_phys: BigEndianU32,
    /// Length in bytes of the strings block
    size_dt_strings: BigEndianU32,
    /// Length in bytes of the struct block
    size_dt_struct: BigEndianU32,
}

impl FdtHeader {
    pub(crate) fn valid_magic(&self) -> bool {
        self.magic.get() == 0xd00dfeed
    }

    /// Ensures structure, strings, and mem-reserve blocks lie inside `limit`.
    pub(crate) fn validate_block_bounds(&self, limit: usize) -> Result<(), crate::FdtError> {
        let totalsize = self.totalsize.get() as usize;
        if totalsize > limit {
            return Err(crate::FdtError::BufferTooSmall);
        }

        self.struct_range_within(totalsize)
            .ok_or(crate::FdtError::BadOffset)?;
        self.strings_range_within(totalsize)
            .ok_or(crate::FdtError::BadOffset)?;
        if (self.off_mem_rsvmap.get() as usize) > totalsize {
            return Err(crate::FdtError::BadOffset);
        }
        Ok(())
    }

    pub(crate) fn struct_range(&self) -> core::ops::Range<usize> {
        self.struct_range_within(usize::MAX).unwrap_or(0..0)
    }

    pub(crate) fn strings_range(&self) -> core::ops::Range<usize> {
        self.strings_range_within(usize::MAX).unwrap_or(0..0)
    }

    pub(crate) fn mem_rsvmap_range(&self) -> core::ops::RangeFrom<usize> {
        (self.off_mem_rsvmap.get() as usize)..
    }

    fn struct_range_within(&self, limit: usize) -> Option<core::ops::Range<usize>> {
        checked_block_range(self.off_dt_struct.get(), self.size_dt_struct.get(), limit)
    }

    fn strings_range_within(&self, limit: usize) -> Option<core::ops::Range<usize>> {
        checked_block_range(self.off_dt_strings.get(), self.size_dt_strings.get(), limit)
    }

    pub(crate) fn from_bytes(bytes: &mut FdtData<'_>) -> Option<Self> {
        Some(Self {
            magic: bytes.u32()?,
            totalsize: bytes.u32()?,
            off_dt_struct: bytes.u32()?,
            off_dt_strings: bytes.u32()?,
            off_mem_rsvmap: bytes.u32()?,
            version: bytes.u32()?,
            last_comp_version: bytes.u32()?,
            boot_cpuid_phys: bytes.u32()?,
            size_dt_strings: bytes.u32()?,
            size_dt_struct: bytes.u32()?,
        })
    }
}

fn checked_block_range(offset: u32, size: u32, limit: usize) -> Option<core::ops::Range<usize>> {
    let start = offset as usize;
    let end = start.checked_add(size as usize)?;
    (end <= limit).then_some(start..end)
}
