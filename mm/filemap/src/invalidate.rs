// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use khal::paging::PageSize;
use memaddr::VirtAddr;
use memspace::InvalidateHandle;
use vmobj::{MappingViewNotifier, ObjectInvalidateRequest, ObjectInvalidateWork, ObjectViewHit};

/// Reverse-mapping notifier for one file-backed VMA view.
pub struct MmSpaceInvalidate {
    handle: InvalidateHandle,
}

impl MmSpaceInvalidate {
    pub fn new(handle: InvalidateHandle) -> Arc<Self> {
        Arc::new(Self { handle })
    }

    fn request(
        work: &ObjectInvalidateWork,
        hit: &ObjectViewHit,
    ) -> KResult<Option<ObjectInvalidateRequest>> {
        let Some(aligned_hit) = hit.aligned_object_suffix(PageSize::Size4K as u64) else {
            return Ok(None);
        };
        work.request_for_subhit(hit, &aligned_hit)
            .map(Some)
            .ok_or(KError::InvalidData)
    }
}

impl MappingViewNotifier for MmSpaceInvalidate {
    fn invalidate(&self, work: &ObjectInvalidateWork, hit: &ObjectViewHit) {
        let request = match Self::request(work, hit) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(err) => {
                warn!(
                    "Ignored file-backed hit {} not carried by its invalidate work: {err}",
                    hit.view().id().raw()
                );
                return;
            }
        };
        let unmap_start = VirtAddr::from_usize(request.hit().vma_start() as usize);
        let unmap_len = request.hit().vma_len();
        match self.handle.submit(request) {
            Ok(_) => {}
            Err(err) => {
                warn!(
                    "Failed to invalidate mapping view {} at {:?} (len={}): {err}",
                    hit.view().id().raw(),
                    unmap_start,
                    unmap_len
                );
            }
        }
    }

    fn invalidate_sync(&self, work: &ObjectInvalidateWork, hit: &ObjectViewHit) -> KResult<()> {
        let Some(request) = Self::request(work, hit)? else {
            return Ok(());
        };
        self.handle.apply_sync(&request)
    }
}
