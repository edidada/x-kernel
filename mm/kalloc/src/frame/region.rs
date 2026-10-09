// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc_engine::{AllocError, AllocResult, BuddyPageAllocator};
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

use super::FrameMeta;

pub(super) const MAX_REGIONS: usize = BuddyPageAllocator::<PAGE_SIZE_4K>::MAX_REGIONS;

/// Boot-time RAM handed to the page allocator through a stable linear mapping.
///
/// Construction only describes addresses. The unsafe initialization boundary
/// verifies arithmetic and requires the caller to guarantee mapping validity,
/// exclusive ownership, and the lifetime of the storage.
#[derive(Clone, Copy, Debug)]
pub struct FrameRegion {
    pub(super) paddr: PhysAddr,
    pub(super) vaddr: VirtAddr,
    pub(super) size_bytes: usize,
}

impl FrameRegion {
    pub(super) const EMPTY: Self = Self::new(PhysAddr::from_usize(0), VirtAddr::from_usize(0), 0);

    /// Describes available RAM and its corresponding mapped kernel address.
    pub const fn new(paddr: PhysAddr, vaddr: VirtAddr, size_bytes: usize) -> Self {
        Self {
            paddr,
            vaddr,
            size_bytes,
        }
    }

    fn normalize(self) -> AllocResult<Self> {
        let pa = self.paddr.as_usize();
        let va = self.vaddr.as_usize();
        let end = pa
            .checked_add(self.size_bytes)
            .ok_or(AllocError::InvalidInput)?;
        va.checked_add(self.size_bytes)
            .ok_or(AllocError::InvalidInput)?;
        if pa % PAGE_SIZE_4K != va % PAGE_SIZE_4K || va < pa {
            return Err(AllocError::InvalidInput);
        }
        let start = pa
            .checked_add(PAGE_SIZE_4K - 1)
            .ok_or(AllocError::InvalidInput)?
            & !(PAGE_SIZE_4K - 1);
        let end = end & !(PAGE_SIZE_4K - 1);
        if start >= end {
            return Ok(Self::EMPTY);
        }
        let mapped = va + (start - pa);
        if mapped == 0 {
            return Err(AllocError::InvalidInput);
        }
        Ok(Self::new(
            PhysAddr::from_usize(start),
            VirtAddr::from_usize(mapped),
            end - start,
        ))
    }
}

/// Accounting for resident descriptors and the initial page allocator inventory.
///
/// These are boot-time quantities, not a live free-memory snapshot. Described
/// pages include both descriptor backing and the buddy allocator's bitmaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameBootstrapInfo {
    /// Number of physical base-page slots covered by the registry.
    pub described_pages: usize,
    /// Pages permanently reserved for the descriptor array.
    pub descriptor_pages: usize,
    /// Pages permanently reserved by the buddy allocator for its own metadata.
    pub buddy_metadata_pages: usize,
    /// Pages initially available to the allocator, before heap bootstrap.
    pub allocatable_pages: usize,
    /// Physical start of the resident descriptor backing.
    pub descriptor_paddr: PhysAddr,
}

pub(super) struct FramePlan {
    pub regions: [FrameRegion; MAX_REGIONS],
    pub count: usize,
    pub described_pages: usize,
    pub backing: FrameRegion,
    pub linear_offset: usize,
}

impl FramePlan {
    pub fn new(input: &[FrameRegion]) -> AllocResult<Self> {
        let mut plan = Self {
            regions: [FrameRegion::EMPTY; MAX_REGIONS],
            count: 0,
            described_pages: 0,
            backing: FrameRegion::EMPTY,
            linear_offset: 0,
        };
        for region in input.iter().copied().filter(|r| r.size_bytes != 0) {
            let region = region.normalize()?;
            if region.size_bytes == 0 {
                continue;
            }
            if plan.count == MAX_REGIONS {
                return Err(AllocError::NoMemory);
            }
            let offset = region.vaddr.as_usize() - region.paddr.as_usize();
            if plan.count == 0 {
                plan.linear_offset = offset;
            } else if plan.linear_offset != offset {
                return Err(AllocError::InvalidInput);
            }
            plan.regions[plan.count] = region;
            plan.count += 1;
        }
        if plan.count == 0 {
            return Err(AllocError::NoMemory);
        }
        plan.regions[..plan.count].sort_unstable_by_key(|r| r.paddr);
        let mut merged = 0;
        for index in 0..plan.count {
            let region = plan.regions[index];
            if merged != 0 {
                let previous = &mut plan.regions[merged - 1];
                let end = previous.paddr.as_usize() + previous.size_bytes;
                if region.paddr.as_usize() < end {
                    return Err(AllocError::MemoryOverlap);
                }
                if region.paddr.as_usize() == end {
                    previous.size_bytes = previous
                        .size_bytes
                        .checked_add(region.size_bytes)
                        .ok_or(AllocError::InvalidInput)?;
                    continue;
                }
            }
            plan.regions[merged] = region;
            merged += 1;
        }
        plan.count = merged;
        for region in &plan.regions[..plan.count] {
            plan.described_pages = plan
                .described_pages
                .checked_add(region.size_bytes / PAGE_SIZE_4K)
                .ok_or(AllocError::InvalidInput)?;
        }
        let backing_bytes = plan
            .described_pages
            .checked_mul(core::mem::size_of::<FrameMeta>())
            .and_then(|bytes| bytes.checked_add(PAGE_SIZE_4K - 1))
            .ok_or(AllocError::InvalidInput)?
            & !(PAGE_SIZE_4K - 1);
        let region = plan.regions[..plan.count]
            .iter()
            .filter(|r| r.size_bytes > backing_bytes)
            .max_by_key(|r| r.size_bytes)
            .ok_or(AllocError::NoMemory)?;
        let offset = region.size_bytes - backing_bytes;
        plan.backing = FrameRegion::new(
            PhysAddr::from_usize(region.paddr.as_usize() + offset),
            VirtAddr::from_usize(region.vaddr.as_usize() + offset),
            backing_bytes,
        );
        Ok(plan)
    }

    pub fn allocator_regions(&self) -> impl Iterator<Item = FrameRegion> + '_ {
        self.regions[..self.count].iter().copied().map(|mut r| {
            let end = r.paddr.as_usize() + r.size_bytes;
            if end == self.backing.paddr.as_usize() + self.backing.size_bytes {
                r.size_bytes -= self.backing.size_bytes;
            }
            r
        })
    }
}

#[cfg(unittest)]
mod tests {
    use alloc_engine::AllocError;
    use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddr, VirtAddr};
    use unittest::def_test;

    use super::{FramePlan, FrameRegion, MAX_REGIONS};

    fn region(start: usize, pages: usize) -> FrameRegion {
        FrameRegion::new(
            PhysAddr::from_usize(start),
            VirtAddr::from_usize(start),
            pages * PAGE,
        )
    }

    #[def_test]
    fn plan_preserves_holes_and_excludes_backing() {
        let plan = FramePlan::new(&[region(512 * PAGE, 64), region(256 * PAGE, 64)]).unwrap();
        assert_eq!(plan.count, 2);
        assert_eq!(plan.described_pages, 128);
        let remaining: usize = plan.allocator_regions().map(|r| r.size_bytes / PAGE).sum();
        assert_eq!(remaining + plan.backing.size_bytes / PAGE, 128);
        assert_eq!(plan.regions[0].paddr.as_usize(), 256 * PAGE);
        assert_eq!(plan.regions[1].paddr.as_usize(), 512 * PAGE);
    }

    #[def_test]
    fn plan_rounds_inward_and_merges_adjacent_ram() {
        let first = FrameRegion::new(
            PhysAddr::from_usize(PAGE + 1),
            VirtAddr::from_usize(PAGE + 1),
            4 * PAGE - 1,
        );
        let plan = FramePlan::new(&[first, region(5 * PAGE, 4)]).unwrap();
        assert_eq!(plan.count, 1);
        assert_eq!(plan.regions[0].paddr.as_usize(), 2 * PAGE);
        assert_eq!(plan.described_pages, 7);
    }

    #[def_test]
    fn plan_rejects_overlap_overflow_and_inconsistent_mapping() {
        assert!(matches!(
            FramePlan::new(&[region(PAGE, 8), region(2 * PAGE, 8)]),
            Err(AllocError::MemoryOverlap)
        ));
        let overflow = FrameRegion::new(
            PhysAddr::from_usize(usize::MAX - PAGE),
            VirtAddr::from_usize(usize::MAX - PAGE),
            2 * PAGE,
        );
        assert!(matches!(
            FramePlan::new(&[overflow]),
            Err(AllocError::InvalidInput)
        ));
        let alias = FrameRegion::new(
            PhysAddr::from_usize(32 * PAGE),
            VirtAddr::from_usize(64 * PAGE),
            8 * PAGE,
        );
        assert!(matches!(
            FramePlan::new(&[region(PAGE, 8), alias]),
            Err(AllocError::InvalidInput)
        ));
    }

    #[def_test]
    fn plan_rejects_empty_tiny_and_excessive_regions() {
        assert!(matches!(FramePlan::new(&[]), Err(AllocError::NoMemory)));
        assert!(matches!(
            FramePlan::new(&[region(PAGE, 1)]),
            Err(AllocError::NoMemory)
        ));
        let regions =
            core::array::from_fn::<_, { MAX_REGIONS + 1 }, _>(|i| region((i * 16 + 1) * PAGE, 8));
        assert!(matches!(
            FramePlan::new(&regions),
            Err(AllocError::NoMemory)
        ));
    }
}
