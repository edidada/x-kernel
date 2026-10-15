// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Transitional bridge between physical frame lifetimes and the VA-based buddy.
//!
//! TODO: remove this module and its call sites together when buddy registration,
//! allocation and deallocation use PA/PFN, and raw VA callers are migrated.
//! Page-content mapping remains a separate concern in frame_mapping.

use alloc_engine::{AllocError, AllocResult, BaseAllocator, BuddyPageAllocator, PageAllocator};
use memaddr::{PAGE_SIZE_4K, PhysAddr};

use crate::{FrameRegion, GlobalAllocator, UsageKind};

pub(crate) fn register_region(
    pages: &mut BuddyPageAllocator<PAGE_SIZE_4K>,
    region: FrameRegion,
) -> AllocResult<usize> {
    let before = pages.total_pages();
    pages.add_region(region.vaddr.as_usize(), region.size_bytes)?;
    // The current buddy excludes a bitmap prefix from its usable page count.
    // A future physical interface must report reservations explicitly.
    let usable = pages.total_pages() - before;
    Ok(region.size_bytes / PAGE_SIZE_4K - usable)
}

pub(crate) fn allocate_frame(
    allocator: &GlobalAllocator,
    kind: UsageKind,
) -> AllocResult<PhysAddr> {
    let mapping = allocator
        .frame_mapping
        .get()
        .ok_or(AllocError::NotInitialized)?;
    let va = allocator.alloc_pages(1, PAGE_SIZE_4K, kind)?;
    let paddr = mapping
        .physical_address(va.into())
        .expect("buddy returned an invalid mapped frame address");
    assert!(
        allocator
            .frame_meta(paddr.as_usize() / PAGE_SIZE_4K)
            .is_some(),
        "buddy returned a page outside resident frame coverage"
    );
    Ok(paddr)
}

pub(crate) fn release_frame(allocator: &GlobalAllocator, paddr: PhysAddr, kind: UsageKind) {
    let va = allocator
        .frame_mapping
        .get()
        .expect("live frame must have a content mapping")
        .kernel_address(paddr)
        .expect("registered frame address must not overflow");
    allocator.dealloc_pages_inner(va.as_usize(), 1, kind);
}

pub(crate) fn validate_raw_release(
    allocator: &GlobalAllocator,
    va: usize,
    pages: usize,
) -> AllocResult {
    if pages == 0 {
        return Ok(());
    }
    if let Some(registry) = allocator.frames.get() {
        let mapping = allocator
            .frame_mapping
            .get()
            .ok_or(AllocError::NotInitialized)?;
        let paddr = mapping
            .physical_address(va.into())
            .ok_or(AllocError::NotFound)?;
        registry.validate_raw_release(paddr.as_usize() / PAGE_SIZE_4K, pages)?;
    }
    Ok(())
}

/// Allocates a complete power-of-two block, including every backing page.
pub(crate) fn allocate_run(
    allocator: &GlobalAllocator,
    num_frames: usize,
    align_pow2: usize,
    kind: UsageKind,
    max_paddr_exclusive: Option<usize>,
) -> AllocResult<PhysAddr> {
    let mapping = allocator
        .frame_mapping
        .get()
        .ok_or(AllocError::NotInitialized)?;
    // Old buddy aligns its VA. Reject mappings that cannot preserve the
    // requested physical alignment; do not silently return misaligned RAM.
    if !mapping.preserves_alignment(align_pow2) {
        return Err(AllocError::InvalidInput);
    }
    let va = if max_paddr_exclusive.is_some() {
        // Preserve DMA's direct buddy selection; PCP may contain higher RAM.
        allocator
            .palloc
            .lock()
            .allocate_pages(num_frames, align_pow2)?
    } else {
        allocator.alloc_pages(num_frames, align_pow2, kind)?
    };
    let paddr = mapping
        .physical_address(va.into())
        .expect("buddy returned an invalid mapped block");
    if let Some(limit) = max_paddr_exclusive {
        let bytes = num_frames * PAGE_SIZE_4K; // checked by alloc_frame_run_below
        if paddr
            .as_usize()
            .checked_add(bytes)
            .is_none_or(|end| end > limit)
        {
            allocator.palloc.lock().deallocate_pages(va, num_frames);
            return Err(AllocError::NoMemory);
        }
        if !matches!(kind, UsageKind::RustHeap) {
            allocator.record_usage_alloc(kind, bytes);
        }
    }
    // Allocation supplied exclusive inventory. Any referenced/permanent or
    // missing slot here means the backend violated the boot coverage contract.
    allocator
        .frames
        .get()
        .expect("frame allocation requires a published registry")
        .validate_raw_release(paddr.as_usize() / PAGE_SIZE_4K, num_frames)
        .expect("buddy returned a run outside unshared physical inventory");
    Ok(paddr)
}
