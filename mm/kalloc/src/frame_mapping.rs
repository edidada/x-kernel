// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Permanent access mapping for the RAM handed to a page allocator.
//!
//! This mapping describes how the kernel accesses page contents. PFN coverage
//! and descriptor lookup belong to FrameRegistry and do not depend on it.

use alloc_engine::{AllocError, AllocResult};
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

use crate::FrameRegion;

pub(crate) struct LinearFrameMapping {
    offset: usize,
}

impl LinearFrameMapping {
    pub(crate) fn new(regions: &[FrameRegion]) -> AllocResult<Self> {
        let first = regions.first().ok_or(AllocError::NoMemory)?;
        let offset = first
            .vaddr
            .as_usize()
            .checked_sub(first.paddr.as_usize())
            .ok_or(AllocError::InvalidInput)?;
        if !offset.is_multiple_of(PAGE_SIZE_4K)
            || regions
                .iter()
                .any(|r| r.vaddr.as_usize().checked_sub(r.paddr.as_usize()) != Some(offset))
        {
            return Err(AllocError::InvalidInput);
        }
        Ok(Self { offset })
    }

    pub(crate) fn kernel_address(&self, paddr: PhysAddr) -> Option<VirtAddr> {
        paddr
            .as_usize()
            .checked_add(self.offset)
            .map(VirtAddr::from_usize)
    }

    // Used by the transitional VA buddy adapter. Delete this reverse conversion
    // when buddy registration, allocation and release use physical identities.
    pub(crate) fn physical_address(&self, vaddr: VirtAddr) -> Option<PhysAddr> {
        let pa = vaddr.as_usize().checked_sub(self.offset)?;
        pa.is_multiple_of(PAGE_SIZE_4K)
            .then(|| PhysAddr::from_usize(pa))
    }
}
