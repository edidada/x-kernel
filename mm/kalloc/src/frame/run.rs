// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{fmt, mem::ManuallyDrop};

use alloc_engine::{AllocError, AllocResult};
use memaddr::{PhysAddr, VirtAddr};

use super::FrameRef;
use crate::{GlobalAllocator, UsageKind};

/// One hold on every base page in a physically contiguous allocation.
///
/// The backing count is a power of two, including buddy rounding slack. Each
/// page has an independent FrameMeta and can be retained by a FrameRef. Dropping
/// the run ends its holds; retained pages are returned only after their own last
/// holders finish. This is not a compound-page or huge-folio protocol.
///
/// The run contains no heap-allocated array of references. Content access and
/// mapping retirement obey the same caller protocols as FrameRef.
pub struct FrameRun<'a> {
    allocator: &'a GlobalAllocator,
    paddr: PhysAddr,
    num_frames: usize,
}

impl<'a> FrameRun<'a> {
    /// # Safety
    /// The complete run is an exclusive, unpublished allocation from allocator,
    /// with num_frames initialized, covered base-page slots and no other owner.
    pub(crate) unsafe fn from_allocation(
        allocator: &'a GlobalAllocator,
        paddr: PhysAddr,
        num_frames: usize,
        kind: UsageKind,
    ) -> Self {
        let first = paddr.as_usize() / memaddr::PAGE_SIZE_4K;
        for index in 0..num_frames {
            allocator
                .frame_meta(first + index)
                .expect("allocated run must be fully described")
                .claim(kind);
        }
        Self {
            allocator,
            paddr,
            num_frames,
        }
    }

    /// Returns the first page's physical address.
    pub fn phys_addr(&self) -> PhysAddr {
        self.paddr
    }

    /// Returns the actual backing count, including rounded-up pages.
    pub fn frame_count(&self) -> usize {
        self.num_frames
    }

    /// Returns the content mapping of the contiguous backing.
    pub fn kernel_addr(&self) -> VirtAddr {
        self.allocator
            .frame_mapping
            .get()
            .expect("live run must have a content mapping")
            .kernel_address(self.paddr)
            .expect("registered run address must not overflow")
    }

    /// Acquires another lifetime hold on one page without allocating metadata.
    ///
    /// # Errors
    /// Returns InvalidInput for an out-of-range index or NoMemory at the
    /// reference-count limit. Existing run ownership remains intact on error.
    pub fn try_clone_frame(&self, index: usize) -> AllocResult<FrameRef<'a>> {
        if index >= self.num_frames {
            return Err(AllocError::InvalidInput);
        }
        let paddr = self.paddr + index * memaddr::PAGE_SIZE_4K;
        let meta = self
            .allocator
            .frame_meta(paddr.as_usize() / memaddr::PAGE_SIZE_4K)
            .expect("live run must remain fully described");
        meta.add_reference()?;
        Ok(FrameRef {
            allocator: self.allocator,
            meta,
            paddr,
        })
    }
}

impl FrameRun<'static> {
    /// Transfers one outstanding hold per backing page without changing counts.
    ///
    /// The recipient must eventually restore the complete run exactly once with
    /// the same allocator and backing count, after hardware users have retired.
    pub fn into_raw(self) -> (PhysAddr, usize) {
        let owner = ManuallyDrop::new(self);
        (owner.paddr, owner.num_frames)
    }

    /// # Safety
    /// This consumes exactly one unreconstructed into_raw transfer of this full
    /// run, from this allocator and the same allocation of every covered page.
    pub(crate) unsafe fn from_transferred_reference(
        allocator: &'static GlobalAllocator,
        paddr: PhysAddr,
        num_frames: usize,
    ) -> Self {
        assert!(paddr.as_usize().is_multiple_of(memaddr::PAGE_SIZE_4K));
        assert!(num_frames.is_power_of_two());
        let bytes = num_frames
            .checked_mul(memaddr::PAGE_SIZE_4K)
            .expect("transferred run byte count must not overflow");
        assert!(bytes <= isize::MAX as usize);
        let end = paddr
            .as_usize()
            .checked_add(bytes)
            .expect("transferred run must not wrap");
        for pa in (paddr.as_usize()..end).step_by(memaddr::PAGE_SIZE_4K) {
            let meta = allocator
                .frame_meta(pa / memaddr::PAGE_SIZE_4K)
                .expect("transferred run must be fully described");
            assert!(
                matches!(meta.state(), super::FrameState::Shared { .. }),
                "transferred run must remain live"
            );
        }
        Self {
            allocator,
            paddr,
            num_frames,
        }
    }
}

impl fmt::Debug for FrameRun<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameRun")
            .field("paddr", &self.paddr)
            .field("num_frames", &self.num_frames)
            .finish()
    }
}

impl Drop for FrameRun<'_> {
    fn drop(&mut self) {
        let first = self.paddr.as_usize() / memaddr::PAGE_SIZE_4K;
        for index in 0..self.num_frames {
            let meta = self
                .allocator
                .frame_meta(first + index)
                .expect("live run must be fully described");
            if meta.remove_reference() {
                let kind = meta.usage_kind();
                meta.finish_retirement();
                // Release base pages individually: some may have independent
                // holders. The current buddy can coalesce free sub-blocks of
                // an allocation without freeing retained siblings.
                self.allocator
                    .release_frame(self.paddr + index * memaddr::PAGE_SIZE_4K, kind);
            }
        }
    }
}
