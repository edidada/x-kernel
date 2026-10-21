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
/// This is a lifetime capability, distinct from the resident FrameMeta. One
/// reference can implement an exclusive allocation owner; cloning shares the
/// same allocation without allocating a control block.
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
            matches!(meta.state(), super::FrameState::Referenced { .. }),
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
unittest::path_mod_test!(tests, "../../ut-tests/frame_reference.rs");
