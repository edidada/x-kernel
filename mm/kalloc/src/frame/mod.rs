// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident descriptors for allocator-managed base pages.
//!
//! A descriptor identifies a physical slot. Finding one does not acquire
//! ownership of the current allocation, and an unshared slot is not necessarily
//! free: legacy allocations and allocator caches still own unshared pages.

mod reference;
mod region;
mod registry;

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use alloc_engine::{AllocError, AllocResult};
pub use reference::FrameRef;
pub use region::{FrameBootstrapInfo, FrameRegion};
pub(crate) use registry::FrameRegistry;

use crate::UsageKind;

const UNSHARED: usize = 0;
const PERMANENT: usize = usize::MAX;
const RETIRING: usize = usize::MAX - 1;
const INITIALIZING: usize = usize::MAX - 2;
const MAX_REFERENCES: usize = isize::MAX as usize;

/// Diagnostic snapshot of a resident descriptor's lifetime state.
///
/// An unshared page may be held by a raw allocation or PCP. These snapshots
/// are not allocation admission checks, and do not acquire a page reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameState {
    /// Lifetime is not managed by a `FrameRef` at this instant.
    Unshared,
    /// Descriptor or buddy metadata storage, never allocatable.
    Permanent,
    /// The allocator is publishing the new allocation's initial reference.
    Initializing,
    /// The current allocation has this many lifetime references.
    Shared {
        /// Snapshot of the number of live holders.
        references: usize,
    },
    /// The unique last holder is returning the page to allocator inventory.
    Retiring,
}

/// Resident metadata for one allocator-managed 4 KiB physical page.
///
/// The storage outlives all allocations of the physical page. This descriptor
/// is not itself a live-page reference; its existence never authorizes a free
/// or access to the page contents.
pub struct FrameMeta {
    refs: AtomicUsize,
    kind: AtomicU8,
}

impl FrameMeta {
    fn new(is_permanent: bool) -> Self {
        Self {
            refs: AtomicUsize::new(if is_permanent { PERMANENT } else { UNSHARED }),
            kind: AtomicU8::new(UsageKind::Global as u8),
        }
    }

    /// Snapshot of the current lifetime state, without acquiring ownership.
    pub fn state(&self) -> FrameState {
        match self.refs.load(Ordering::Acquire) {
            UNSHARED => FrameState::Unshared,
            PERMANENT => FrameState::Permanent,
            RETIRING => FrameState::Retiring,
            INITIALIZING => FrameState::Initializing,
            references => FrameState::Shared { references },
        }
    }

    fn claim(&self, kind: UsageKind) {
        self.refs
            .compare_exchange(UNSHARED, INITIALIZING, Ordering::Acquire, Ordering::Relaxed)
            .expect("allocator returned an already referenced or permanent frame");
        // Allocation ownership excludes old holders. Publishing the first
        // reference releases the category initialized for this incarnation.
        self.kind.store(kind as u8, Ordering::Relaxed);
        self.refs.store(1, Ordering::Release);
    }

    fn add_reference(&self) -> AllocResult {
        match self
            .refs
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |refs| {
                if (1..MAX_REFERENCES).contains(&refs) {
                    Some(refs + 1)
                } else {
                    None
                }
            }) {
            Ok(_) => Ok(()),
            Err(MAX_REFERENCES) => Err(AllocError::NoMemory),
            Err(_) => panic!("only a live frame reference can be cloned"),
        }
    }

    fn remove_reference(&self) -> bool {
        let mut previous = self.refs.load(Ordering::Relaxed);
        loop {
            assert!(
                (1..=MAX_REFERENCES).contains(&previous),
                "invalid frame reference release"
            );
            let next = if previous == 1 {
                RETIRING
            } else {
                previous - 1
            };
            // The last holder acquires prior holders' release operations
            // before returning the storage for another allocation.
            let ordering = if previous == 1 {
                Ordering::AcqRel
            } else {
                Ordering::Release
            };
            match self
                .refs
                .compare_exchange_weak(previous, next, ordering, Ordering::Relaxed)
            {
                Ok(_) => return previous == 1,
                Err(current) => previous = current,
            }
        }
    }

    fn usage_kind(&self) -> UsageKind {
        use strum::VariantArray;
        // Only an existing holder (or the unique retiring holder) reads this
        // field, so the allocation's category cannot be replaced concurrently.
        UsageKind::VARIANTS[self.kind.load(Ordering::Relaxed) as usize]
    }

    fn finish_retirement(&self) {
        assert_eq!(self.refs.load(Ordering::Relaxed), RETIRING);
        // No new reference can be acquired from a bare descriptor. Inventory
        // is published only after this store, and nothing resets it afterward.
        self.refs.store(UNSHARED, Ordering::Release);
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
