// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Folio allocation, data and dirty/writeback state.

use kalloc::{FrameRef, UsageKind, global_allocator};
use kerrno::{KError, KResult};
use khal::mem::PhysAddr;
use log::warn;
use memaddr::PAGE_SIZE_4K;

use crate::PageIndex;

/// Cached folio stored inside a [`crate::MappingPages`].
#[derive(Debug)]
pub struct Folio {
    index: PageIndex,
    frame: FrameRef<'static>,
    uptodate: bool,
    readahead: bool,
    dirty: bool,
    is_invalidated: bool,
    writeback: bool,
}

impl Folio {
    /// Allocates a non-uptodate, zero-filled folio with a stable index.
    pub fn new_zeroed(index: PageIndex) -> KResult<Self> {
        let frame = global_allocator()
            .alloc_zeroed_frame(UsageKind::PageCache)
            .map_err(|_| KError::NoMemory)?;
        Ok(Self {
            index,
            frame,
            uptodate: false,
            readahead: false,
            dirty: false,
            is_invalidated: false,
            writeback: false,
        })
    }

    /// Returns whether backing data has been initialized successfully.
    pub fn is_uptodate(&self) -> bool {
        self.uptodate
    }

    /// Makes a completely initialized folio available to cache readers.
    pub fn mark_uptodate(&mut self) {
        self.uptodate = true;
    }

    /// Marks this newly reserved page as a trigger for the next read window.
    pub fn mark_readahead(&mut self) {
        self.readahead = true;
    }

    /// Returns whether this folio carries a shared read-window trigger.
    pub fn is_readahead(&self) -> bool {
        self.readahead
    }

    /// Consumes a window trigger while holding the folio lock.
    /// Writeback pages retain the marker for a later read.
    pub fn take_readahead(&mut self) -> bool {
        if self.writeback {
            return false;
        }
        core::mem::take(&mut self.readahead)
    }

    /// Returns the immutable page index; this does not prove mapping residency.
    pub const fn index(&self) -> PageIndex {
        self.index
    }

    /// Marks a removed cache entry unusable by previously collected readers.
    pub(crate) fn invalidate(&mut self) {
        self.is_invalidated = true;
        self.clear_dirty();
    }

    pub(crate) fn is_invalidated(&self) -> bool {
        self.is_invalidated
    }

    /// Returns the physical address of this folio.
    pub fn paddr(&self) -> PhysAddr {
        self.frame.phys_addr()
    }

    /// Acquires a lifetime hold for a mapping or another page user.
    ///
    /// This keeps the physical allocation alive even after the folio leaves
    /// the cache. It does not retain cache residency, dirty/writeback state or
    /// exclusive data access. Mapping owners retain it through TLB retirement.
    ///
    /// # Errors
    /// Returns `NoMemory` if the lifetime reference limit would be exceeded.
    pub fn try_clone_frame(&self) -> KResult<FrameRef<'static>> {
        self.frame.try_clone_ref().map_err(|_| KError::NoMemory)
    }

    /// Marks the folio dirty.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Returns whether the folio is currently dirty.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Returns whether a writeback pass is writing this folio.
    pub fn is_under_writeback(&self) -> bool {
        self.writeback
    }

    pub(super) fn start_writeback(&mut self) {
        debug_assert!(!self.writeback);
        self.writeback = true;
    }

    pub(super) fn end_writeback(&mut self) {
        debug_assert!(self.writeback);
        self.writeback = false;
    }

    /// Clears the dirty bit after successful writeback or truncation cleanup.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Returns the folio contents as a mutable 4 KiB slice.
    pub fn data(&mut self) -> &mut [u8] {
        // SAFETY: the frame retains a mapped 4 KiB allocation for this borrow.
        // Cache-side access is serialized by the folio's mutable borrow/lock;
        // additional lifetime holds do not expose safe Rust data references.
        unsafe {
            core::slice::from_raw_parts_mut(self.frame.kernel_addr().as_mut_ptr(), PAGE_SIZE_4K)
        }
    }
}

impl Drop for Folio {
    fn drop(&mut self) {
        if self.dirty || self.writeback {
            warn!(
                "dropping folio {} with dirty={} writeback={}",
                self.index, self.dirty, self.writeback
            );
        }
        // The frame field ends the cache hold after diagnostic checks.
        // Mapping holds can keep the physical page alive independently.
    }
}
