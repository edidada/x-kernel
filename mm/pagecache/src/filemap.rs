// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio lookup and materialization.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use ksync::Mutex;
use memaddr::PAGE_SIZE_4K;

use crate::{Folio, FolioBatch, MappingPages, PageIndex};

impl MappingPages {
    /// Collects up to `count` contiguous resident folios starting at `start`.
    ///
    /// Stops at the first hole or [`FolioBatch::CAPACITY`] without loading pages.
    /// References keep allocations alive; batch access runs outside the mapping lock.
    pub fn get_folios_contig(&self, start: PageIndex, count: usize) -> FolioBatch {
        self.get_folios_contig_with_run_len(start, count.min(FolioBatch::CAPACITY))
            .0
    }

    /// Collects references and counts the contiguous resident prefix in one traversal.
    ///
    /// Returns at most [`FolioBatch::CAPACITY`] references and a count up to `count`,
    /// checking entries beyond capacity without retaining them. This mapping-lock
    /// snapshot does not guarantee residency during later access.
    pub fn get_folios_contig_with_run_len(
        &self,
        start: PageIndex,
        count: usize,
    ) -> (FolioBatch, usize) {
        let mut batch = FolioBatch::new();
        if count == 0 {
            return (batch, 0);
        }
        let mut run_len = 0;
        let state = self.state.lock();
        for (&index, folio) in state.entries.range(start..).take(count) {
            if start.checked_add(run_len as u64) != Some(index) {
                break;
            }
            if !batch.is_full() {
                batch.push(folio.clone());
            }
            run_len += 1;
        }
        (batch, run_len)
    }

    /// Runs `f` with the cached folio at `index`, if present.
    ///
    /// Retries removed candidates; calls `f` once under the folio lock.
    /// The callback must not re-enter this mapping; dirty changes are reconciled on return.
    pub fn with_folio<R>(&self, index: PageIndex, f: impl FnOnce(Option<&mut Folio>) -> R) -> R {
        let mut callback = Some(f);
        loop {
            let folio = self.state.lock().entries.get(&index).cloned();
            let Some(folio) = folio else {
                return callback.take().expect("callback has not run")(None);
            };
            if let Some(result) = self.with_valid_folio(&folio, |locked| {
                callback.take().expect("callback has not run")(Some(locked))
            }) {
                return result;
            }
        }
    }

    /// Runs `f` with a folio at `index`, materializing one on demand.
    ///
    /// Retries removed candidates, possibly repeating the factory; `f` runs once
    /// under the folio lock and dirty changes are reconciled. Neither callback may
    /// re-enter this mapping. The owner must serialize reads and publication against truncate.
    ///
    /// # Errors
    ///
    /// Propagates factory/callback errors. A factory folio with a different
    /// index is rejected with `InvalidInput` before publication.
    pub fn with_folio_or_create<R>(
        &self,
        index: PageIndex,
        mut instantiate_folio: impl FnMut(PageIndex) -> KResult<Folio>,
        f: impl FnOnce(&mut Folio) -> KResult<R>,
    ) -> KResult<R> {
        let mut callback = Some(f);
        loop {
            let folio = {
                let mut state = self.state.lock();
                if let Some(folio) = state.entries.get(&index) {
                    folio.clone()
                } else {
                    let folio = instantiate_folio(index)?;
                    if folio.index() != index {
                        return Err(KError::InvalidInput);
                    }
                    let folio = Arc::new(Mutex::new(folio));
                    state.entries.insert(index, folio.clone());
                    folio
                }
            };
            if let Some(result) = self.with_valid_folio(&folio, |locked| {
                callback.take().expect("callback has not run")(locked)
            }) {
                return result;
            }
        }
    }

    fn with_valid_folio<R>(
        &self,
        folio: &Arc<Mutex<Folio>>,
        f: impl FnOnce(&mut Folio) -> R,
    ) -> Option<R> {
        let (result, needs_dirty_reconciliation, index) = {
            let mut locked = folio.lock();
            if locked.is_invalidated() {
                return None;
            }
            let was_dirty = locked.is_dirty();
            let result = f(&mut locked);
            let is_dirty = locked.is_dirty();
            (result, was_dirty != is_dirty, locked.index())
        };
        if needs_dirty_reconciliation {
            self.reconcile_dirty_folio(index, folio);
        }
        Some(result)
    }

    /// Adds one page-cache folio from backing-store data if it is still absent.
    ///
    /// This is the page-cache insertion side used by readahead after the
    /// backing filesystem has already filled a byte range.
    pub fn filemap_add_folio(&self, index: PageIndex, offset: usize, src: &[u8]) -> KResult<bool> {
        let end = offset.checked_add(src.len()).ok_or(KError::InvalidInput)?;
        if end > PAGE_SIZE_4K {
            return Err(KError::InvalidInput);
        }
        let mut folio = Folio::new_zeroed(index)?;
        folio.data()[offset..end].copy_from_slice(src);
        let mut state = self.state.lock();
        if state.entries.contains_key(&index) {
            return Ok(false);
        }
        state.entries.insert(index, Arc::new(Mutex::new(folio)));
        Ok(true)
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn collected_folios_reject_removed_pages() {
        let cache = MappingPages::new();
        let count = FolioBatch::CAPACITY + 2;
        for index in 0..count {
            cache.filemap_add_folio(index as u64, 0, b"old").unwrap();
        }
        let (batch, run_len) = cache.get_folios_contig_with_run_len(0, count);
        assert_eq!(batch.len(), FolioBatch::CAPACITY);
        assert_eq!(run_len, count);
        // Interleave removal/replacement after lookup and before folio locking.
        let candidate = cache.state.lock().entries[&0].clone();
        cache.resize_cached_folios((count * PAGE_SIZE_4K) as u64, 0);
        cache.filemap_add_folio(0, 0, b"new").unwrap();
        assert_eq!(batch.with_read_folio(0, |_| ()), None);
        assert_eq!(
            cache.with_valid_folio(&candidate, |_| panic!("replaced callback")),
            None::<()>
        );
        assert_eq!(
            cache.with_folio(0, |folio| folio.unwrap().data()[..3].to_vec()),
            b"new"
        );
    }
}
