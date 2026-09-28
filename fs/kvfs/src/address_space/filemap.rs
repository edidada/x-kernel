// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio access and generic buffered I/O.

use alloc::sync::Arc;

use iov_iter::{IovIterDest, IovIterSource};
use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;
use pagecache::{Folio, PageIndex};

use super::{AddressSpace, WriteBeginRequest, WriteEndRequest};
use crate::{Kiocb, VfsError, VfsResult};

fn write_prefix_or_error(written: usize, error: VfsError) -> VfsResult<usize> {
    if written == 0 {
        Err(error)
    } else {
        Ok(written)
    }
}

impl AddressSpace {
    /// Runs `f` with the cached folio at `index`, if present.
    pub fn with_folio<R>(&self, index: PageIndex, f: impl FnOnce(Option<&mut Folio>) -> R) -> R {
        self.i_pages.with_folio(index, f)
    }

    /// Runs `f` with the folio at `index`, materializing it when absent.
    ///
    /// The shared invalidate lock covers loading and the callback. When attached
    /// to an inode, rejects pages starting at or beyond EOF under that lock.
    /// The callback must not reenter this mapping or fault on user memory.
    ///
    /// # Errors
    ///
    /// Returns `BadAddress` at or beyond inode EOF, `InvalidInput` for an invalid
    /// index, or propagates folio materialization and callback errors.
    pub fn with_folio_or_create<R>(
        &self,
        index: PageIndex,
        f: impl FnOnce(&mut Folio) -> KResult<R>,
    ) -> KResult<R> {
        let inode = self.inode();
        let page_start = index
            .checked_mul(PAGE_SIZE_4K as u64)
            .ok_or(VfsError::InvalidInput)?;
        let _invalidate_guard = self.invalidate_lock.read();
        if inode
            .as_ref()
            .is_some_and(|inode| page_start >= inode.size())
        {
            return Err(VfsError::BadAddress);
        }
        self.with_folio_or_create_invalidate_locked(index, f)
    }

    // Shared invalidate or exclusive inode data locking excludes ordinary truncate.
    // Final eviction instead requires no active inode users.
    fn with_folio_or_create_invalidate_locked<R>(
        &self,
        index: PageIndex,
        f: impl FnOnce(&mut Folio) -> KResult<R>,
    ) -> KResult<R> {
        self.i_pages.with_folio_or_create(
            index,
            |index| {
                let mut folio = Folio::new_zeroed(index)?;
                let file_len = self.inode().map(|inode| inode.size());
                let read = self.read_folio(&mut folio)?;
                if read > PAGE_SIZE_4K {
                    return Err(VfsError::InvalidData);
                }
                let page_start = index
                    .checked_mul(PAGE_SIZE_4K as u64)
                    .ok_or(VfsError::InvalidInput)?;
                if let Some(file_len) = file_len {
                    let required = file_len.saturating_sub(page_start).min(PAGE_SIZE_4K as u64);
                    if (read as u64) < required {
                        return Err(VfsError::Io);
                    }
                }
                folio.data()[read..].fill(0);
                Ok(folio)
            },
            f,
        )
    }

    /// Prepares and dirties a cached folio for a shared write fault.
    ///
    /// The address-space invalidate lock excludes truncate and cache
    /// invalidation, while the folio lock covers the EOF recheck, filesystem
    /// preparation, dirtying, and the caller's PTE update.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::BadAddress`] if truncate moved the folio beyond EOF,
    /// or propagates filesystem preparation, dirtying, and PTE-update errors.
    pub(crate) fn page_mkwrite<R>(
        &self,
        index: PageIndex,
        prepare: impl FnOnce(u64, usize) -> VfsResult<()>,
        f: impl FnOnce(&mut Folio) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let pos = index
            .checked_mul(PAGE_SIZE_4K as u64)
            .ok_or(VfsError::InvalidInput)?;
        let _invalidate_guard = self.invalidate_lock.read();
        self.with_folio_or_create_invalidate_locked(index, |folio| {
            let file_len = inode.size();
            if pos >= file_len {
                return Err(VfsError::BadAddress);
            }
            let len = usize::try_from((file_len - pos).min(PAGE_SIZE_4K as u64))
                .map_err(|_| VfsError::InvalidInput)?;
            prepare(pos, len)?;
            self.dirty_folio(folio)?;
            f(folio)
        })
    }

    fn read_folio(&self, folio: &mut Folio) -> VfsResult<usize> {
        self.ops.read_folio(self, folio)
    }

    fn dirty_folio(&self, folio: &mut Folio) -> VfsResult<bool> {
        self.ops.dirty_folio(self, folio)
    }

    /// Performs a buffered read from this address space into an iterator.
    pub(crate) fn read_iter(
        self: &Arc<Self>,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterDest<'_>,
    ) -> VfsResult<usize> {
        let file_len = self
            .inode()
            .map(|inode| inode.size())
            .ok_or(VfsError::InvalidInput)?;
        let pos = iocb.ki_pos();
        if iter.count() == 0 || pos >= file_len {
            return Ok(0);
        }

        let read = self.read_from_page_cache(iter, pos, file_len)?;
        iocb.advance(read);
        Ok(read)
    }

    pub(super) fn read_from_page_cache(
        self: &Arc<Self>,
        iter: &mut IovIterDest<'_>,
        offset: u64,
        file_len: u64,
    ) -> VfsResult<usize> {
        let mut total = 0;
        match self.read_cache_batches(iter, offset, file_len, &mut total) {
            Ok(()) => Ok(total),
            Err(error) if total == 0 => Err(error),
            Err(_) => Ok(total),
        }
    }

    fn read_cache_batches(
        self: &Arc<Self>,
        iter: &mut IovIterDest<'_>,
        mut offset: u64,
        file_len: u64,
        total: &mut usize,
    ) -> VfsResult<()> {
        let inode = self.inode();
        let current_file_len = || {
            inode
                .as_ref()
                .map_or(file_len, |inode| inode.size().min(file_len))
        };
        if iter.count() == 0 || offset >= current_file_len() {
            return Ok(());
        }
        if iter.count() <= PAGE_SIZE_4K - (offset % PAGE_SIZE_4K as u64) as usize {
            *total = self.copy_folio_to_iter(iter, offset, &current_file_len)?;
            return Ok(());
        }
        let mut prepared_end = offset;
        while iter.count() != 0 && offset < current_file_len() {
            let batch = if offset >= prepared_end {
                let (end, batch) =
                    self.prepare_read_window_and_batch(offset, iter.count(), current_file_len())?;
                prepared_end = end;
                batch
            } else {
                let start = offset / PAGE_SIZE_4K as u64;
                // Stay within this window to avoid rescanning the next one.
                let end = prepared_end.min(current_file_len());
                let count = end.div_ceil(PAGE_SIZE_4K as u64).saturating_sub(start) as usize;
                self.i_pages.get_folios_contig(start, count)
            };
            for slot in 0..batch.len().max(1) {
                if offset >= current_file_len() {
                    return Ok(());
                }
                let page_remaining = PAGE_SIZE_4K - (offset % PAGE_SIZE_4K as u64) as usize;
                let copied = match batch.with_read_folio(slot, |data| {
                    Self::copy_cached_bytes(iter, data, offset, current_file_len())
                }) {
                    Some(result) => result?,
                    None => self.copy_folio_to_iter(iter, offset, &current_file_len)?,
                };
                offset += copied as u64;
                *total += copied;
                if copied < page_remaining || iter.count() == 0 || offset >= current_file_len() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn copy_folio_to_iter(
        &self,
        iter: &mut IovIterDest<'_>,
        offset: u64,
        current_file_len: &impl Fn() -> u64,
    ) -> VfsResult<usize> {
        let index = offset / PAGE_SIZE_4K as u64;
        loop {
            if let Some(result) = self.i_pages.with_folio(index, |folio| {
                folio.map(|folio| {
                    Self::copy_cached_bytes(iter, folio.data(), offset, current_file_len())
                })
            }) {
                return result;
            }
            // Exclude truncate during loading, but release before user copy can fault or block.
            let _invalidate_guard = self.invalidate_lock.read();
            if offset >= current_file_len() {
                return Ok(0);
            }
            self.with_folio_or_create_invalidate_locked(index, |_| Ok(()))?;
        }
    }

    fn copy_cached_bytes(
        iter: &mut IovIterDest<'_>,
        data: &[u8],
        offset: u64,
        file_len: u64,
    ) -> VfsResult<usize> {
        let page_off = (offset % PAGE_SIZE_4K as u64) as usize;
        let step = Self::copy_len(offset, file_len);
        iter.copy_to_iter(&data[page_off..page_off + step])
    }

    fn copy_len(offset: u64, file_len: u64) -> usize {
        let page_off = (offset % PAGE_SIZE_4K as u64) as usize;
        file_len
            .saturating_sub(offset)
            .min((PAGE_SIZE_4K - page_off) as u64) as usize
    }

    /// Performs a buffered write from an iterator into this address space.
    pub(crate) fn write_iter(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterSource<'_>,
    ) -> VfsResult<usize> {
        self.write_iter_with_checks(iocb, iter, |_, _| Ok(()))
    }

    pub(crate) fn write_iter_with_checks(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterSource<'_>,
        checks: impl FnOnce(u64, &mut usize) -> VfsResult<()>,
    ) -> VfsResult<usize> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let _data_guard = inode.lock_data();
        let mut count = iter.count();
        self.generic_write_check_limits(iocb, &mut count)?;
        if count != 0 {
            checks(iocb.ki_pos(), &mut count)?;
        }
        iter.truncate(count);
        let count = iter.count();
        if count == 0 {
            return Ok(0);
        }

        let written = self.perform_write(iocb.ki_pos(), iter, count)?;
        iocb.advance(written);
        Ok(written)
    }

    fn generic_write_check_limits(&self, iocb: &Kiocb<'_>, count: &mut usize) -> VfsResult<()> {
        if *count == 0 {
            return Ok(());
        }

        let max_file_size = iocb.file().max_file_size();
        let pos = iocb.ki_pos();
        if pos >= max_file_size {
            return Err(VfsError::FileTooLarge);
        }

        let count_u64 = u64::try_from(*count).map_err(|_| VfsError::InvalidInput)?;
        let count_u64 = count_u64.min(max_file_size - pos);
        *count = usize::try_from(count_u64).map_err(|_| VfsError::InvalidInput)?;
        Ok(())
    }

    // The caller holds the inode data lock through rollback to keep size and
    // competing writes stable. Rollback may wait for concurrent writeback;
    // filesystem callbacks have returned before cache shrink, so their locks
    // are released. Writeback completion must not depend on the data lock.
    fn perform_write(
        &self,
        offset: u64,
        iter: &mut IovIterSource<'_>,
        len: usize,
    ) -> VfsResult<usize> {
        let mut written = 0usize;
        let mut visible_len = self.visible_len()?;
        while written < len {
            let pos = offset
                .checked_add(written as u64)
                .ok_or(VfsError::InvalidInput)?;
            let index = pos / memaddr::PAGE_SIZE_4K as u64;
            let page_off = (pos % memaddr::PAGE_SIZE_4K as u64) as usize;
            let step = (len - written).min(memaddr::PAGE_SIZE_4K - page_off);
            let prepared_end = pos.checked_add(step as u64).ok_or(VfsError::InvalidInput)?;
            if let Err(error) = self
                .ops
                .write_begin(self, WriteBeginRequest::new(pos, step))
            {
                return write_prefix_or_error(written, error);
            }
            let grew_cache = prepared_end > visible_len;
            if grew_cache {
                self.i_pages.resize_cached_folios(visible_len, prepared_end);
            }
            let mut copied = 0;
            let copy_result = self.with_folio_or_create_invalidate_locked(index, |folio| {
                let data = folio.data();
                copied = iter.copy_from_iter(&mut data[page_off..page_off + step])?;
                if copied != 0 {
                    self.dirty_folio(folio)?;
                }
                Ok(())
            });
            if let Err(error) = copy_result {
                iter.revert(copied)?;
                if let Err(cancel_error) =
                    self.ops.write_end(self, WriteEndRequest::new(pos, step, 0))
                {
                    log::warn!(
                        "failed to cancel buffered write at {pos} (len {step}): {cancel_error:?}"
                    );
                }
                if grew_cache {
                    self.i_pages.resize_cached_folios(prepared_end, visible_len);
                }
                return write_prefix_or_error(written, error);
            }
            let accepted = match self
                .ops
                .write_end(self, WriteEndRequest::new(pos, step, copied))
            {
                Ok(accepted) => accepted,
                Err(error) => {
                    iter.revert(copied)?;
                    if grew_cache {
                        self.i_pages.resize_cached_folios(prepared_end, visible_len);
                    }
                    return write_prefix_or_error(written, error);
                }
            };
            if accepted > copied {
                return Err(VfsError::InvalidInput);
            }
            if accepted < copied {
                iter.revert(copied - accepted)?;
            }
            // `write_end` publishes EOF as `pos + accepted`; roll the cache back
            // to that same end so unaccepted bytes are not left cached and dirty
            // beyond the visible size.
            let accepted_end = pos
                .checked_add(accepted as u64)
                .ok_or(VfsError::InvalidInput)?;
            if grew_cache && accepted_end < prepared_end {
                self.i_pages
                    .resize_cached_folios(prepared_end, accepted_end);
            }
            visible_len = visible_len.max(accepted_end);
            if accepted == 0 {
                break;
            }
            written += accepted;
            if accepted < step {
                break;
            }
        }

        Ok(written)
    }
}
