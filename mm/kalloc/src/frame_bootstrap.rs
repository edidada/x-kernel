// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot-owned RAM planning, descriptor storage reservation and backend handoff.
//!
//! Physical coverage and runtime lifetime state belong to the frame module.

use core::ptr::NonNull;

use alloc_engine::{AllocError, AllocResult, BuddyPageAllocator, PageAllocator};
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

use crate::{
    buddy_frame_adapter,
    frame::{FrameLayout, FrameRegistry, MAX_FRAME_REGIONS},
    frame_mapping::LinearFrameMapping,
};

const MAX_REGIONS: usize = MAX_FRAME_REGIONS;

/// Boot-time RAM handed to the current backend through stable kernel mappings.
///
/// Construction only describes addresses. The unsafe initialization boundary
/// verifies arithmetic and requires the caller to guarantee mapping validity,
/// exclusive ownership, and the lifetime of the storage.
#[derive(Clone, Copy, Debug)]
pub struct FrameRegion {
    pub(crate) paddr: PhysAddr,
    pub(crate) vaddr: VirtAddr,
    pub(crate) size_bytes: usize,
}

impl FrameRegion {
    pub(crate) const EMPTY: Self = Self::new(PhysAddr::from_usize(0), VirtAddr::from_usize(0), 0);

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
        if pa % PAGE_SIZE_4K != va % PAGE_SIZE_4K {
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

struct FramePlan {
    regions: [FrameRegion; MAX_REGIONS],
    count: usize,
    layout: FrameLayout,
    backing: FrameRegion,
}

impl FramePlan {
    fn new(input: &[FrameRegion]) -> AllocResult<Self> {
        let mut plan = Self {
            regions: [FrameRegion::EMPTY; MAX_REGIONS],
            count: 0,
            layout: FrameLayout::EMPTY,
            backing: FrameRegion::EMPTY,
        };
        for region in input.iter().copied().filter(|r| r.size_bytes != 0) {
            let region = region.normalize()?;
            if region.size_bytes == 0 {
                continue;
            }
            if plan.count == MAX_REGIONS {
                return Err(AllocError::NoMemory);
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
                if region.paddr.as_usize() == end
                    && previous.vaddr.as_usize() + previous.size_bytes == region.vaddr.as_usize()
                {
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
        plan.layout = FrameLayout::new(
            plan.regions[..plan.count]
                .iter()
                .map(|r| (r.paddr, r.size_bytes)),
        )?;
        let backing_bytes = plan
            .layout
            .described_pages()
            .checked_mul(core::mem::size_of::<crate::FrameMeta>())
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

    fn into_inventory(self) -> (FrameLayout, FrameRegion, impl Iterator<Item = FrameRegion>) {
        let backing = self.backing;
        let inventory = self.regions.into_iter().take(self.count).map(move |mut r| {
            let end = r.paddr.as_usize() + r.size_bytes;
            if end == backing.paddr.as_usize() + backing.size_bytes {
                r.size_bytes -= backing.size_bytes;
            }
            r
        });
        (self.layout, backing, inventory)
    }
}

/// Prepares resident storage and hands non-reserved inventory to the legacy buddy.
///
/// # Safety
/// Regions must be exclusive, permanently mapped writable RAM, valid for the
/// allocator's lifetime. The buddy must be empty and boot registration must be
/// serialized against every allocation entry point.
pub(crate) unsafe fn bootstrap(
    regions: &[FrameRegion],
    pages: &mut BuddyPageAllocator<PAGE_SIZE_4K>,
) -> AllocResult<(FrameRegistry, LinearFrameMapping, FrameBootstrapInfo)> {
    let plan = FramePlan::new(regions)?;
    // Validate the current content mapping before writing RAM. This restriction
    // belongs to the boot/backend handoff, not the physical descriptor layout.
    let mapping = LinearFrameMapping::new(&plan.regions[..plan.count])?;
    let described_pages = plan.layout.described_pages();
    let (layout, backing, inventory) = plan.into_inventory();
    let descriptors =
        NonNull::new(backing.vaddr.as_mut_ptr().cast()).ok_or(AllocError::InvalidInput)?;
    // SAFETY: planning supplies enough aligned, exclusive writable backing,
    // excludes it from buddy inventory, and the boot owner keeps it mapped and
    // alive for the allocator's lifetime. The physical layout needs no RAM VA.
    let registry = unsafe { FrameRegistry::initialize(layout, descriptors, backing.size_bytes) }?;
    registry.reserve(
        backing.paddr.as_usize() / PAGE_SIZE_4K,
        backing.size_bytes / PAGE_SIZE_4K,
    );
    let mut buddy_metadata_pages = 0;
    for region in inventory {
        let reserved = match buddy_frame_adapter::register_region(pages, region) {
            Ok(reserved) => reserved,
            Err(error) => {
                // Nothing is published and no allocation has occurred yet.
                pages.reset();
                return Err(error);
            }
        };
        registry.reserve(region.paddr.as_usize() / PAGE_SIZE_4K, reserved);
        buddy_metadata_pages += reserved;
    }
    let info = FrameBootstrapInfo {
        described_pages,
        descriptor_pages: backing.size_bytes / PAGE_SIZE_4K,
        buddy_metadata_pages,
        allocatable_pages: pages.total_pages(),
        descriptor_paddr: backing.paddr,
    };
    Ok((registry, mapping, info))
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/frame_bootstrap.rs");

#[cfg(unittest)]
unittest::path_mod_test!(
    integration_tests,
    "../ut-tests/frame_bootstrap_integration.rs"
);
