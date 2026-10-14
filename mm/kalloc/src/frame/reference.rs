// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{fmt, mem::ManuallyDrop};

use alloc_engine::AllocResult;
use memaddr::{PhysAddr, VirtAddr};

use super::FrameMeta;
use crate::{GlobalAllocator, UsageKind};

/// One lifetime reference to an allocator-managed 4 KiB physical page.
///
/// Cloning shares the same allocation without allocating a control block.
/// The last reference returns the page to its allocator exactly once. This
/// capability keeps storage alive; it does not grant exclusive content access
/// or authorize installing or removing a hardware mapping.
///
/// Mapping owners must retain their reference through the required translation
/// invalidation boundary. Merely clearing a PTE is not a release boundary.
/// Allocation and final release have the allocator's context restrictions:
/// they may run in ordinary or IRQ context, never NMI, and do not sleep.
pub struct FrameRef<'a> {
    pub(super) allocator: &'a GlobalAllocator,
    pub(super) meta: &'a FrameMeta,
    pub(super) paddr: PhysAddr,
}

impl<'a> FrameRef<'a> {
    /// # Safety
    /// This is an exclusive, unpublished one-page allocation from allocator.
    /// No other owner or raw-reference transfer exists for this incarnation.
    pub(crate) unsafe fn from_allocation(
        allocator: &'a GlobalAllocator,
        paddr: PhysAddr,
        kind: UsageKind,
    ) -> Self {
        let meta = allocator
            .frame_meta(paddr.as_usize() / memaddr::PAGE_SIZE_4K)
            .expect("allocated frame must have a resident descriptor");
        meta.claim(kind);
        Self {
            allocator,
            meta,
            paddr,
        }
    }

    /// Physical start address of this live base page.
    pub fn phys_addr(&self) -> PhysAddr {
        self.paddr
    }

    /// Physical base-page number of this live allocation.
    pub fn pfn(&self) -> usize {
        self.paddr.as_usize() / memaddr::PAGE_SIZE_4K
    }

    /// Stable kernel address of the page through the registered linear mapping.
    ///
    /// The address does not grant mutable Rust access. The caller must uphold
    /// the content-access protocol when using a pointer derived from it.
    pub fn kernel_addr(&self) -> VirtAddr {
        self.allocator
            .frame_mapping
            .get()
            .expect("live frame must have a content mapping")
            .kernel_address(self.paddr)
            .expect("registered frame address must not overflow")
    }

    /// Allocation category charged until the last lifetime reference ends.
    pub fn usage_kind(&self) -> UsageKind {
        self.meta.usage_kind()
    }

    /// Acquires another reference to the same allocation without allocating.
    ///
    /// Returns `NoMemory` if the reference-count limit would be exceeded. This
    /// method requires an existing live reference and cannot revive a page from
    /// a bare PFN or descriptor borrow.
    pub fn try_clone_ref(&self) -> AllocResult<Self> {
        self.meta.add_reference()?;
        Ok(Self {
            allocator: self.allocator,
            meta: self.meta,
            paddr: self.paddr,
        })
    }
}

impl FrameRef<'static> {
    /// # Safety
    /// Consumes exactly one live into_raw transfer from this allocator and
    /// allocation incarnation; it does not acquire an additional reference.
    pub(crate) unsafe fn from_transferred_reference(
        allocator: &'static GlobalAllocator,
        paddr: PhysAddr,
    ) -> Self {
        assert!(paddr.as_usize().is_multiple_of(memaddr::PAGE_SIZE_4K));
        let meta = allocator
            .frame_meta(paddr.as_usize() / memaddr::PAGE_SIZE_4K)
            .expect("raw frame reference must belong to this allocator");
        assert!(
            matches!(meta.state(), super::FrameState::Shared { .. }),
            "raw reference must still be live"
        );
        Self {
            allocator,
            meta,
            paddr,
        }
    }

    /// Transfers this reference to a raw physical-address owner.
    ///
    /// The count is unchanged. The recipient must eventually consume this
    /// reference exactly once with the same allocator's `frame_from_raw` and
    /// retain it through any mapping or device-use completion. Only references
    /// to static allocators can be transferred without a Rust lifetime token.
    pub fn into_raw(self) -> PhysAddr {
        let owner = ManuallyDrop::new(self);
        owner.paddr
    }
}

impl fmt::Debug for FrameRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameRef")
            .field("paddr", &self.paddr)
            .field("kind", &self.usage_kind())
            .finish()
    }
}

impl Drop for FrameRef<'_> {
    fn drop(&mut self) {
        if self.meta.remove_reference() {
            let kind = self.usage_kind();
            self.meta.finish_retirement();
            self.allocator.release_frame(self.paddr, kind);
        }
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::boxed::Box;
    use core::sync::atomic::Ordering;

    use alloc_engine::AllocError;
    use kspin::NoPreemptIrqSave;
    use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddr};
    use unittest::def_test;

    use super::super::{FrameState, MAX_REFERENCES};
    use crate::{FrameRegion, GlobalAllocator, GlobalPage, UsageKind, global_allocator};

    struct Fixture {
        // Field order drops the local registry before returning its backing.
        allocator: Box<GlobalAllocator>,
        storage: GlobalPage,
    }

    impl Fixture {
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: test scenario: reserve and initialize \
                      exclusive fixture RAM before testing lifetime operations"
        )]
        fn new() -> Self {
            let storage = GlobalPage::alloc_contiguous(64, PAGE).unwrap();
            let base = storage.start_va();
            let region = FrameRegion::new(
                PhysAddr::from_usize(kaddr_layout::v2p(base.as_usize())),
                base,
                64 * PAGE,
            );
            let allocator = Box::new(GlobalAllocator::new());
            // SAFETY: this fixture exclusively owns its writable backing,
            // retains it until allocator teardown, and initializes only once.
            unsafe { allocator.init_page_allocator_with_regions(&[region]) }.unwrap();
            Self { allocator, storage }
        }
    }

    fn counters(kind: UsageKind) -> (usize, usize) {
        crate::with_current_usage_counters(|counters| {
            let index = kind as usize;
            (
                counters.allocated_bytes[index].load(Ordering::Relaxed),
                counters.freed_bytes[index].load(Ordering::Relaxed),
            )
        })
    }

    #[def_test]
    fn shared_frame_returns_only_after_last_reference() {
        let fixture = Fixture::new();
        let _guard = NoPreemptIrqSave::new();
        let before = counters(UsageKind::Global);
        let owner = fixture.allocator.alloc_frame(UsageKind::Global).unwrap();
        let va = owner.kernel_addr();
        let meta = fixture.allocator.frame_meta(owner.pfn()).unwrap();
        let second = owner.try_clone_ref().unwrap();
        assert_eq!(meta.state(), FrameState::Shared { references: 2 });
        drop(owner);
        assert_eq!(meta.state(), FrameState::Shared { references: 1 });
        // A raw caller cannot put the same physical storage into PCP/buddy.
        assert_eq!(
            fixture
                .allocator
                .validate_raw_page_release(va.as_usize(), 1),
            Err(AllocError::NotAllocated)
        );
        assert_eq!(counters(UsageKind::Global), (before.0 + PAGE, before.1));
        drop(second);
        assert_eq!(meta.state(), FrameState::Unshared);
        assert_eq!(
            counters(UsageKind::Global),
            (before.0 + PAGE, before.1 + PAGE)
        );
        let reused = fixture.allocator.alloc_frame(UsageKind::Global).unwrap();
        assert_eq!(reused.kernel_addr(), va);
    }

    #[def_test]
    fn frame_zeroing_and_heap_category_accounting() {
        let fixture = Fixture::new();
        let _guard = NoPreemptIrqSave::new();
        let before = counters(UsageKind::RustHeap);
        let owner = fixture.allocator.alloc_frame(UsageKind::RustHeap).unwrap();
        // SAFETY: this is the fixture's only reference, never published to a
        // mapping or another reader; exactly one valid base page is written.
        unsafe { core::ptr::write_bytes(owner.kernel_addr().as_mut_ptr(), 0xa5, PAGE) };
        let va = owner.kernel_addr();
        drop(owner);
        let zeroed = fixture
            .allocator
            .alloc_zeroed_frame(UsageKind::RustHeap)
            .unwrap();
        assert_eq!(zeroed.kernel_addr(), va);
        // SAFETY: this private fixture has not published or cloned the page;
        // the temporary read-only slice is bounded by the live owner.
        let bytes = unsafe { core::slice::from_raw_parts(zeroed.kernel_addr().as_ptr(), PAGE) };
        assert!(bytes.iter().all(|byte| *byte == 0));
        assert_eq!(
            counters(UsageKind::RustHeap),
            (before.0 + 2 * PAGE, before.1 + PAGE)
        );
        drop(zeroed);
        assert_eq!(
            counters(UsageKind::RustHeap),
            (before.0 + 2 * PAGE, before.1 + 2 * PAGE)
        );
    }

    #[def_test]
    fn raw_range_checks_every_slot_and_permanent_backing() {
        let fixture = Fixture::new();
        let owner = fixture.allocator.alloc_frame(UsageKind::Global).unwrap();
        let va = owner.kernel_addr().as_usize();
        assert_eq!(
            fixture.allocator.validate_raw_page_release(va, 1),
            Err(AllocError::NotAllocated)
        );
        if fixture.allocator.frame_meta(owner.pfn() - 1).is_some() {
            assert!(
                fixture
                    .allocator
                    .validate_raw_page_release(va - PAGE, 2)
                    .is_err()
            );
        }
        let registry = fixture.allocator.frames.get().unwrap();
        let backing = fixture
            .allocator
            .frame_meta(kaddr_layout::v2p(fixture.storage.start_va().as_usize()) / PAGE + 63)
            .unwrap();
        assert!(backing.is_permanent());
        assert_eq!(
            registry.validate_raw_release(
                kaddr_layout::v2p(fixture.storage.start_va().as_usize()) / PAGE + 63,
                1
            ),
            Err(AllocError::NotAllocated)
        );
    }

    #[def_test]
    fn reference_overflow_preserves_the_existing_owner() {
        let fixture = Fixture::new();
        let owner = fixture.allocator.alloc_frame(UsageKind::Global).unwrap();
        // This fixture is private and has exactly one holder. Simulate the
        // count boundary, then restore its real count before destruction.
        owner.meta.refs.store(MAX_REFERENCES, Ordering::Relaxed);
        assert!(matches!(owner.try_clone_ref(), Err(AllocError::NoMemory)));
        assert_eq!(
            owner.meta.state(),
            FrameState::Shared {
                references: MAX_REFERENCES
            }
        );
        owner.meta.refs.store(1, Ordering::Relaxed);
    }

    #[def_test]
    fn raw_reference_roundtrip_does_not_increment_or_release_early() {
        let allocator = global_allocator();
        let owner = allocator.alloc_frame(UsageKind::PageTable).unwrap();
        let pfn = owner.pfn();
        let second = owner.try_clone_ref().unwrap();
        let raw = owner.into_raw();
        assert_eq!(
            allocator.frame_meta(pfn).unwrap().state(),
            FrameState::Shared { references: 2 }
        );
        // SAFETY: raw owns precisely the reference just transferred by
        // into_raw, from this same static allocator and allocation instance.
        let restored = unsafe { allocator.frame_from_raw(raw) };
        drop(restored);
        assert_eq!(
            allocator.frame_meta(pfn).unwrap().state(),
            FrameState::Shared { references: 1 }
        );
        drop(second);
    }
}
