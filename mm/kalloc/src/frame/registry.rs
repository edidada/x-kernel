// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{ptr::NonNull, sync::atomic::Ordering};

use alloc_engine::{AllocError, AllocResult};
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

use super::{
    FrameMeta, PERMANENT, UNSHARED,
    region::{FramePlan, MAX_REGIONS},
};

#[derive(Clone, Copy)]
struct Segment {
    start_pfn: usize,
    end_pfn: usize,
    first_index: usize,
}

impl Segment {
    const EMPTY: Self = Self {
        start_pfn: 0,
        end_pfn: 0,
        first_index: 0,
    };
}

pub(crate) struct FrameRegistry {
    segments: [Segment; MAX_REGIONS],
    count: usize,
    descriptors: NonNull<FrameMeta>,
    linear_offset: usize,
}

// SAFETY: the directory is immutable after construction. The initialized
// descriptor backing remains valid for the owning allocator's lifetime, and
// mutable descriptor state is accessed only through atomics.
unsafe impl Send for FrameRegistry {}
// SAFETY: shared lookups borrow initialized descriptors from permanently
// reserved storage; no non-atomic mutable state is exposed after publication.
unsafe impl Sync for FrameRegistry {}

impl FrameRegistry {
    /// # Safety
    /// Every planned region must be exclusively owned writable RAM at its
    /// described mapping, and the backing must outlive the owning allocator.
    /// No allocator or caller may use the descriptor backing after handoff.
    pub(super) unsafe fn initialize(plan: &FramePlan) -> Self {
        let descriptors = NonNull::new(plan.backing.vaddr.as_mut_ptr().cast::<FrameMeta>())
            .expect("validated descriptor backing must be non-null");
        let mut registry = Self {
            segments: [Segment::EMPTY; MAX_REGIONS],
            count: plan.count,
            descriptors,
            linear_offset: plan.linear_offset,
        };
        let mut index = 0;
        for (segment, region) in registry.segments[..plan.count]
            .iter_mut()
            .zip(&plan.regions[..plan.count])
        {
            let start_pfn = region.paddr.as_usize() / PAGE_SIZE_4K;
            let pages = region.size_bytes / PAGE_SIZE_4K;
            *segment = Segment {
                start_pfn,
                end_pfn: start_pfn + pages,
                first_index: index,
            };
            for _ in 0..pages {
                // SAFETY: planning reserved a page-aligned array large enough
                // for every covered slot. Exclusive boot ownership permits
                // initializing each descriptor once before publication.
                unsafe { descriptors.as_ptr().add(index).write(FrameMeta::new(false)) };
                index += 1;
            }
        }
        registry.reserve(plan.backing.vaddr, plan.backing.size_bytes / PAGE_SIZE_4K);
        registry
    }

    pub(crate) fn lookup(&self, pfn: usize) -> Option<&FrameMeta> {
        let segments = &self.segments[..self.count];
        let upper = segments.partition_point(|segment| segment.start_pfn <= pfn);
        let segment = segments.get(upper.checked_sub(1)?)?;
        if pfn >= segment.end_pfn {
            return None;
        }
        let index = segment.first_index + (pfn - segment.start_pfn);
        // SAFETY: the segment bounds select an initialized slot in the
        // reserved array, whose lifetime covers this registry borrow.
        Some(unsafe { &*self.descriptors.as_ptr().add(index) })
    }

    pub(crate) fn pfn_for_va(&self, vaddr: VirtAddr) -> Option<usize> {
        let pa = vaddr.as_usize().checked_sub(self.linear_offset)?;
        if !pa.is_multiple_of(PAGE_SIZE_4K) {
            return None;
        }
        let pfn = pa / PAGE_SIZE_4K;
        self.lookup(pfn).map(|_| pfn)
    }

    pub(crate) fn mapped_address(&self, pfn: usize) -> VirtAddr {
        debug_assert!(self.lookup(pfn).is_some());
        VirtAddr::from_usize(pfn * PAGE_SIZE_4K + self.linear_offset)
    }

    pub(crate) fn physical_address(&self, vaddr: VirtAddr) -> Option<PhysAddr> {
        self.pfn_for_va(vaddr)
            .map(|pfn| PhysAddr::from_usize(pfn * PAGE_SIZE_4K))
    }

    pub(crate) fn validate_raw_release(&self, start: VirtAddr, pages: usize) -> AllocResult {
        if pages == 0 {
            return Ok(());
        }
        let pfn = self.pfn_for_va(start).ok_or(AllocError::NotFound)?;
        let end_pfn = pfn.checked_add(pages).ok_or(AllocError::InvalidInput)?;
        let segments = &self.segments[..self.count];
        let upper = segments.partition_point(|segment| segment.start_pfn <= pfn);
        let segment = &segments[upper - 1];
        if end_pfn > segment.end_pfn {
            return Err(AllocError::NotFound);
        }
        let first = segment.first_index + pfn - segment.start_pfn;
        for index in first..first + pages {
            // SAFETY: the checked range lies within one initialized segment;
            // metadata backing outlives the registry and only atomics are read.
            let meta = unsafe { &*self.descriptors.as_ptr().add(index) };
            if meta.refs.load(Ordering::Acquire) != UNSHARED {
                return Err(AllocError::NotAllocated);
            }
        }
        Ok(())
    }

    pub(crate) fn reserve(&self, start: VirtAddr, pages: usize) {
        let pfn = self
            .pfn_for_va(start)
            .expect("reserved range must be covered");
        for index in 0..pages {
            self.lookup(pfn + index)
                .expect("reserved range must remain covered")
                .refs
                .store(PERMANENT, Ordering::Relaxed);
        }
    }
}
