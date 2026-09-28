// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Readahead requests, window selection and filesystem dispatch.

use alloc::sync::Arc;

use memaddr::PAGE_SIZE_4K;
use pagecache::{FolioBatch, PageIndex};

use super::AddressSpace;
use crate::{RwLockReadGuard, VfsError, VfsResult};

const MAX_READAHEAD_BYTES: usize = 128 * 1024;

/// Synchronous readahead window protected against backing-store truncation.
///
/// Holds the shared invalidate guard through backing reads and publication;
/// validates completions against the initial EOF. Completion is synchronous.
pub struct ReadaheadControl<'a> {
    address_space: &'a AddressSpace,
    _invalidate_guard: RwLockReadGuard<'a, ()>,
    start_index: PageIndex,
    count: usize,
    file_len: Option<u64>,
}

impl<'a> ReadaheadControl<'a> {
    fn new(address_space: &'a AddressSpace, start_index: PageIndex, count: usize) -> Self {
        let invalidate_guard = address_space.invalidate_lock.read();
        let file_len = address_space.inode().map(|inode| inode.size());
        let count = file_len.map_or(count, |file_len| {
            let remaining = file_len
                .div_ceil(PAGE_SIZE_4K as u64)
                .saturating_sub(start_index);
            count.min(usize::try_from(remaining).unwrap_or(usize::MAX))
        });
        Self {
            address_space,
            _invalidate_guard: invalidate_guard,
            start_index,
            count,
            file_len,
        }
    }

    /// Returns the first page index in the requested window.
    pub fn start_index(&self) -> PageIndex {
        self.start_index
    }

    /// Returns the number of pages in the requested window.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Inserts one completed readahead folio if it is still absent.
    ///
    /// Returns `false` outside the window or when a cached folio already exists.
    /// Existing folios take precedence without validating unused completion data.
    ///
    /// # Errors
    ///
    /// Returns `Io` for missing bytes below the initial EOF; propagates allocation and range errors.
    pub fn complete_folio(&self, index: PageIndex, offset: usize, src: &[u8]) -> VfsResult<bool> {
        let count = u64::try_from(self.count).map_err(|_| VfsError::InvalidInput)?;
        let end = self
            .start_index
            .checked_add(count)
            .ok_or(VfsError::InvalidInput)?;
        if index < self.start_index || index >= end {
            return Ok(false);
        }
        if self.address_space.i_pages.cached_run_len(index, 1) != 0 {
            return Ok(false);
        }
        if let Some(file_len) = self.file_len {
            let page_start = index
                .checked_mul(PAGE_SIZE_4K as u64)
                .ok_or(VfsError::InvalidInput)?;
            let required = file_len.saturating_sub(page_start).min(PAGE_SIZE_4K as u64) as usize;
            if required != 0 && (offset != 0 || src.len() < required) {
                return Err(VfsError::Io);
            }
        }
        self.address_space
            .i_pages
            .filemap_add_folio(index, offset, src)
    }
}

impl AddressSpace {
    /// Prepares a window while collecting its first resident read batch.
    ///
    /// Combines hit checking and collection; misses trigger readahead and recollection.
    pub(super) fn prepare_read_window_and_batch(
        self: &Arc<Self>,
        offset: u64,
        len: usize,
        file_len: u64,
    ) -> VfsResult<(u64, FolioBatch)> {
        let read_len =
            (file_len.saturating_sub(offset).min(len as u64) as usize).min(MAX_READAHEAD_BYTES);
        let start_index = offset / PAGE_SIZE_4K as u64;
        let end = offset + read_len as u64;
        let count = if read_len == 0 {
            0
        } else {
            (end.div_ceil(PAGE_SIZE_4K as u64) - start_index) as usize
        };
        let (batch, run_len) = self
            .i_pages
            .get_folios_contig_with_run_len(start_index, count);
        if count > 1 && run_len < count {
            // Recollect because readahead or removal may change the resident prefix.
            drop(batch);
            self.readahead(ReadaheadControl::new(self, start_index, count))?;
            return Ok((end, self.i_pages.get_folios_contig(start_index, count)));
        }
        Ok((end, batch))
    }

    /// Starts readahead for a range of folios.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` for a control belonging to another address space,
    /// or propagates synchronous backing-read and completion errors.
    pub fn readahead(&self, control: ReadaheadControl<'_>) -> VfsResult<()> {
        if !core::ptr::eq(self, control.address_space) {
            return Err(VfsError::InvalidInput);
        }
        if control.count() == 0 {
            return Ok(());
        }
        self.ops.readahead(self, control)
    }
}
