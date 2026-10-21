// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{ptr::NonNull, sync::atomic::Ordering};

use alloc_engine::{AllocError, AllocResult};
use memaddr::{PAGE_SIZE_4K, PhysAddr};

use super::{FrameMeta, PERMANENT, UNREFERENCED};

pub(crate) const MAX_FRAME_REGIONS: usize = 64;

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

/// Physical coverage and packed descriptor indexes, independent of RAM mappings.
pub(crate) struct FrameLayout {
    segments: [Segment; MAX_FRAME_REGIONS],
    count: usize,
    described_pages: usize,
}

impl FrameLayout {
    pub(crate) const EMPTY: Self = Self {
        segments: [Segment::EMPTY; MAX_FRAME_REGIONS],
        count: 0,
        described_pages: 0,
    };

    pub(crate) fn new(ranges: impl Iterator<Item = (PhysAddr, usize)>) -> AllocResult<Self> {
        let mut layout = Self::EMPTY;
        for (start, size_bytes) in ranges.filter(|(_, size)| *size != 0) {
            if !start.as_usize().is_multiple_of(PAGE_SIZE_4K)
                || !size_bytes.is_multiple_of(PAGE_SIZE_4K)
            {
                return Err(AllocError::InvalidInput);
            }
            let end = start
                .as_usize()
                .checked_add(size_bytes)
                .ok_or(AllocError::InvalidInput)?;
            if layout.count == MAX_FRAME_REGIONS {
                return Err(AllocError::NoMemory);
            }
            layout.segments[layout.count] = Segment {
                start_pfn: start.as_usize() / PAGE_SIZE_4K,
                end_pfn: end / PAGE_SIZE_4K,
                first_index: 0,
            };
            layout.count += 1;
        }
        if layout.count == 0 {
            return Err(AllocError::NoMemory);
        }
        layout.segments[..layout.count].sort_unstable_by_key(|s| s.start_pfn);
        let mut merged = 0;
        for index in 0..layout.count {
            let segment = layout.segments[index];
            if merged != 0 {
                let previous = &mut layout.segments[merged - 1];
                if segment.start_pfn < previous.end_pfn {
                    return Err(AllocError::MemoryOverlap);
                }
                if segment.start_pfn == previous.end_pfn {
                    previous.end_pfn = segment.end_pfn;
                    continue;
                }
            }
            layout.segments[merged] = segment;
            merged += 1;
        }
        layout.count = merged;
        for segment in &mut layout.segments[..layout.count] {
            segment.first_index = layout.described_pages;
            layout.described_pages = layout
                .described_pages
                .checked_add(segment.end_pfn - segment.start_pfn)
                .ok_or(AllocError::InvalidInput)?;
        }
        Ok(layout)
    }

    pub(crate) fn described_pages(&self) -> usize {
        self.described_pages
    }

    fn segment_for_pfn(&self, pfn: usize) -> Option<&Segment> {
        let segments = &self.segments[..self.count];
        let upper = segments.partition_point(|segment| segment.start_pfn <= pfn);
        let segment = segments.get(upper.checked_sub(1)?)?;
        (pfn < segment.end_pfn).then_some(segment)
    }
}

pub(crate) struct FrameRegistry {
    layout: FrameLayout,
    descriptors: NonNull<FrameMeta>,
}

// SAFETY: the directory is immutable after construction. The supplied descriptor
// storage remains valid for the allocator's lifetime, and state uses atomics.
unsafe impl Send for FrameRegistry {}
// SAFETY: shared lookups borrow initialized descriptors from permanently reserved
// storage; no non-atomic mutable state is exposed after publication.
unsafe impl Sync for FrameRegistry {}

impl FrameRegistry {
    /// Initializes descriptors in storage supplied by the boot owner.
    ///
    /// # Safety
    /// The pointer must identify exclusive writable storage of backing_bytes
    /// bytes, valid for the allocator's lifetime. The caller must permanently
    /// exclude that storage from allocation. Covered physical pages need not
    /// have a kernel content mapping for descriptor initialization or lookup.
    pub(crate) unsafe fn initialize(
        layout: FrameLayout,
        descriptors: NonNull<FrameMeta>,
        backing_bytes: usize,
    ) -> AllocResult<Self> {
        let bytes = layout
            .described_pages
            .checked_mul(core::mem::size_of::<FrameMeta>())
            .ok_or(AllocError::InvalidInput)?;
        if bytes > backing_bytes
            || bytes > isize::MAX as usize
            || !descriptors.as_ptr().is_aligned()
        {
            return Err(AllocError::InvalidInput);
        }
        for index in 0..layout.described_pages {
            // SAFETY: the checked capacity and alignment select an in-bounds
            // slot in the caller's exclusive storage. Each slot is written once
            // before any references to descriptors are published.
            let slot = unsafe { descriptors.as_ptr().add(index) };
            // SAFETY: the checked slot is aligned, writable and exclusively
            // owned; constructing it does not read uninitialized storage.
            unsafe { slot.write(FrameMeta::new(false)) };
        }
        Ok(Self {
            layout,
            descriptors,
        })
    }

    pub(crate) fn lookup(&self, pfn: usize) -> Option<&FrameMeta> {
        let segment = self.layout.segment_for_pfn(pfn)?;
        let index = segment.first_index + (pfn - segment.start_pfn);
        // SAFETY: the physical directory selects an initialized slot within
        // supplied storage, whose lifetime covers this registry borrow.
        let slot = unsafe { self.descriptors.as_ptr().add(index) };
        // SAFETY: initialization constructed this selected slot and the backing
        // outlives the registry borrow. State changes use atomics only.
        Some(unsafe { &*slot })
    }

    pub(crate) fn validate_raw_release(&self, start_pfn: usize, pages: usize) -> AllocResult {
        if pages == 0 {
            return Ok(());
        }
        let segment = self
            .layout
            .segment_for_pfn(start_pfn)
            .ok_or(AllocError::NotFound)?;
        let end_pfn = start_pfn
            .checked_add(pages)
            .ok_or(AllocError::InvalidInput)?;
        if end_pfn > segment.end_pfn {
            return Err(AllocError::NotFound);
        }
        let first = segment.first_index + start_pfn - segment.start_pfn;
        for index in first..first + pages {
            // SAFETY: the checked range lies within one initialized segment;
            // descriptor storage remains valid and only atomics are read.
            let slot = unsafe { self.descriptors.as_ptr().add(index) };
            // SAFETY: the selected slot was initialized and its backing remains
            // valid; sharing it only exposes atomic lifetime state.
            let meta = unsafe { &*slot };
            if meta.refs.load(Ordering::Acquire) != UNREFERENCED {
                return Err(AllocError::NotAllocated);
            }
        }
        Ok(())
    }

    pub(crate) fn reserve(&self, start_pfn: usize, pages: usize) {
        if pages == 0 {
            return;
        }
        let segment = self
            .layout
            .segment_for_pfn(start_pfn)
            .expect("reserved range must be covered");
        assert!(
            start_pfn
                .checked_add(pages)
                .is_some_and(|end| end <= segment.end_pfn)
        );
        for index in 0..pages {
            self.lookup(start_pfn + index)
                .expect("reserved range must remain covered")
                .refs
                .store(PERMANENT, Ordering::Relaxed);
        }
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../../ut-tests/frame_registry.rs");
