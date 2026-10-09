// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident descriptors for allocator-managed base pages.
//!
//! A descriptor identifies a physical slot. Finding one does not acquire
//! ownership of the current allocation, and an unshared slot is not necessarily
//! free: legacy allocations and allocator caches still own unshared pages.

mod region;
mod registry;

use core::sync::atomic::{AtomicUsize, Ordering};

pub use region::{FrameBootstrapInfo, FrameRegion};
pub(crate) use registry::FrameRegistry;

const UNSHARED: usize = 0;
const PERMANENT: usize = usize::MAX;

/// Resident metadata for one allocator-managed 4 KiB physical page.
///
/// The storage outlives all allocations of the physical page. This descriptor
/// is not itself a live-page reference; its existence never authorizes a free
/// or access to the page contents.
pub struct FrameMeta {
    refs: AtomicUsize,
}

impl FrameMeta {
    fn new(is_permanent: bool) -> Self {
        Self {
            refs: AtomicUsize::new(if is_permanent { PERMANENT } else { UNSHARED }),
        }
    }

    /// Whether this slot backs permanent allocator metadata.
    ///
    /// The flag is established before the registry is published. Such pages
    /// remain mapped but are excluded from allocatable memory.
    pub fn is_permanent(&self) -> bool {
        self.refs.load(Ordering::Relaxed) == PERMANENT
    }
}

/// Prepares resident storage and registers only non-reserved inventory.
///
/// # Safety
/// The regions must describe exclusive, permanently mapped writable RAM whose
/// lifetime covers the allocator. The page allocator must still be empty and
/// boot registration must be serialized against all allocation entry points.
pub(crate) unsafe fn bootstrap(
    regions: &[FrameRegion],
    pages: &mut alloc_engine::BuddyPageAllocator<{ memaddr::PAGE_SIZE_4K }>,
) -> alloc_engine::AllocResult<(FrameRegistry, FrameBootstrapInfo)> {
    use alloc_engine::{BaseAllocator, PageAllocator};
    use memaddr::PAGE_SIZE_4K;

    let plan = region::FramePlan::new(regions)?;
    // SAFETY: the caller owns every planned region through a valid writable
    // mapping. Planning excluded the backing from inventory and verified its
    // array capacity; boot serialization prevents concurrent accesses.
    let registry = unsafe { FrameRegistry::initialize(&plan) };
    let mut buddy_metadata_pages = 0;
    for region in plan.allocator_regions() {
        let before = pages.total_pages();
        if let Err(error) = pages.add_region(region.vaddr.as_usize(), region.size_bytes) {
            // No public allocation or registry publication has happened yet.
            pages.reset();
            return Err(error);
        }
        let usable = pages.total_pages() - before;
        let reserved = region.size_bytes / PAGE_SIZE_4K - usable;
        registry.reserve(region.vaddr, reserved);
        buddy_metadata_pages += reserved;
    }
    let info = FrameBootstrapInfo {
        described_pages: plan.described_pages,
        descriptor_pages: plan.backing.size_bytes / PAGE_SIZE_4K,
        buddy_metadata_pages,
        allocatable_pages: pages.total_pages(),
        descriptor_paddr: plan.backing.paddr,
    };
    Ok((registry, info))
}

#[cfg(unittest)]
mod tests {
    use alloc::boxed::Box;

    use alloc_engine::AllocError;
    use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddr};
    use unittest::def_test;

    use super::FrameRegion;
    use crate::{GlobalAllocator, GlobalPage};

    #[def_test]
    fn bootstrap_keeps_holes_and_metadata_out_of_inventory() {
        let storage = GlobalPage::alloc_contiguous(256, PAGE).unwrap();
        let base = storage.start_va();
        let physical = kaddr_layout::v2p(base.as_usize());
        let regions = [
            FrameRegion::new(PhysAddr::from_usize(physical), base, 64 * PAGE),
            FrameRegion::new(
                PhysAddr::from_usize(physical + 128 * PAGE),
                base + 128 * PAGE,
                64 * PAGE,
            ),
        ];
        let allocator = Box::new(GlobalAllocator::new());
        // SAFETY: storage owns all these mapped pages, outlives allocator, and
        // no other local allocator can access the fixture during initialization.
        let info = unsafe { allocator.init_page_allocator_with_regions(&regions) }.unwrap();
        let pfn = physical / PAGE;
        assert!(allocator.frame_meta(pfn).is_some());
        assert!(allocator.frame_meta(pfn + 64).is_none());
        assert!(allocator.frame_meta(pfn + 128).is_some());
        assert!(
            allocator
                .frame_meta(info.descriptor_paddr.as_usize() / PAGE)
                .unwrap()
                .is_permanent()
        );
        assert_eq!(info.described_pages, 128);
        assert_eq!(
            info.described_pages,
            info.descriptor_pages + info.buddy_metadata_pages + info.allocatable_pages
        );
        assert_eq!(allocator.available_pages(), info.allocatable_pages);
        // SAFETY: the same fixture is still owned; repeated initialization is
        // rejected before any backing storage is changed.
        let repeated = unsafe { allocator.init_page_allocator_with_regions(&regions) };
        assert_eq!(repeated, Err(AllocError::AlreadyInitialized));
    }

    #[def_test]
    fn bootstrap_reserves_buddy_bitmaps_as_well_as_descriptors() {
        let storage = GlobalPage::alloc_contiguous(512, PAGE).unwrap();
        let base = storage.start_va();
        let physical = kaddr_layout::v2p(base.as_usize());
        let region = FrameRegion::new(PhysAddr::from_usize(physical), base, 512 * PAGE);
        let allocator = Box::new(GlobalAllocator::new());
        // SAFETY: the fixture's exclusive mapped storage outlives allocator,
        // and this is its only initialization.
        let info = unsafe { allocator.init_page_allocator_with_regions(&[region]) }.unwrap();
        assert!(info.buddy_metadata_pages > 0);
        assert!(
            allocator
                .frame_meta(physical / PAGE)
                .unwrap()
                .is_permanent()
        );
        assert_eq!(
            info.described_pages,
            info.descriptor_pages + info.buddy_metadata_pages + info.allocatable_pages
        );
    }
}
