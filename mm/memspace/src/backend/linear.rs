// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linear mapping backend.
use alloc::sync::Arc;

use kalloc::{FrameRef, FrameRun};
use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize, PageTableMut, PagingError};
use ksync::Mutex;
use memaddr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, PhysAddrRange, VirtAddr, VirtAddrRange};

use crate::{
    FaultContext, ForkCloneTarget, InvalidateHandle, MmSpace, VmBackingInfo, VmBackingKind,
    backend::{BackendOps, FaultCompletion, FaultCompletionResult, map_paging_err, pages_in},
    vma::VmRuntimeOps,
};

struct OwnedLinearPages {
    frames: FrameRun<'static>,
    visible_size: usize,
}

/// Constant-offset physical mapping, optionally retaining managed RAM backing.
///
/// External mappings rely on their caller's backing lifetime. Owned mappings
/// keep the source run and acquire independent PTE holds, including after fork.
#[derive(Clone)]
pub struct LinearBackend {
    offset: isize,
    backing: Option<Arc<OwnedLinearPages>>,
}

impl LinearBackend {
    pub fn new(offset: isize) -> Self {
        Self {
            offset,
            backing: None,
        }
    }

    /// Consumes RAM backing while exposing only the requested page-aligned prefix.
    /// The original linear mapping classification and fork sharing are retained.
    pub(crate) fn new_owned(
        start: VirtAddr,
        frames: FrameRun<'static>,
        size: usize,
    ) -> KResult<Self> {
        if size == 0
            || !start.is_aligned_4k()
            || !size.is_multiple_of(PAGE_SIZE_4K)
            || size / PAGE_SIZE_4K > frames.frame_count()
            || start.as_usize().checked_add(size).is_none()
        {
            return Err(KError::InvalidInput);
        }
        let physical =
            isize::try_from(frames.phys_addr().as_usize()).map_err(|_| KError::InvalidInput)?;
        let offset = (start.as_usize() as isize)
            .checked_sub(physical)
            .ok_or(KError::InvalidInput)?;
        Ok(Self {
            offset,
            backing: Some(Arc::new(OwnedLinearPages {
                frames,
                visible_size: size,
            })),
        })
    }

    pub(crate) fn can_merge_with(&self, other: &Self) -> bool {
        if self.offset != other.offset {
            return false;
        }
        match (&self.backing, &other.backing) {
            (None, None) => true,
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }

    pub fn clone_for_fork_runtime(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pgtbl: &mut PageTableMut,
        _new_pgtbl: &mut PageTableMut,
        _new_aspace: &Arc<Mutex<MmSpace>>,
        _invalidate: Option<InvalidateHandle>,
    ) -> KResult<Self> {
        Ok(self.clone())
    }

    fn pa(&self, va: VirtAddr) -> KResult<PhysAddr> {
        let translated = (va.as_usize() as isize)
            .checked_sub(self.offset)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(KError::BadAddress)?;
        if let Some(backing) = &self.backing {
            let offset = translated
                .checked_sub(backing.frames.phys_addr().as_usize())
                .ok_or(KError::BadAddress)?;
            if offset >= backing.visible_size {
                return Err(KError::BadAddress);
            }
        }
        Ok(PhysAddr::from(translated))
    }

    fn clone_mapping_frame(&self, va: VirtAddr) -> KResult<FrameRef<'static>> {
        let backing = self.backing.as_ref().ok_or(KError::BadState)?;
        let offset = self.pa(va)?.as_usize() - backing.frames.phys_addr().as_usize();
        backing
            .frames
            .try_clone_frame(offset / PAGE_SIZE_4K)
            .map_err(|_| KError::NoMemory)
    }

    fn map_page(&self, va: VirtAddr, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        if self.backing.is_some() {
            pgtbl
                .map_owned(va, self.clone_mapping_frame(va)?, flags)
                .map_err(map_paging_err)
        } else {
            pgtbl
                .map(va, self.pa(va)?, PageSize::Size4K, flags)
                .map_err(map_paging_err)
        }
    }

    fn map_owned_range(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult {
        let pages = pages_in(range, PageSize::Size4K)?;
        if range.size() > 0 {
            self.pa(range.start)?;
            self.pa(range.end - PAGE_SIZE_4K)?;
        }
        let mut mapped_bytes = 0;
        for va in pages {
            if let Err(error) = self.map_page(va, flags, pgtbl) {
                // Only withdraw the successfully installed prefix; an occupied
                // conflicting leaf belongs to its prior owner. Keep its hold.
                let rollback = pgtbl
                    .unmap_sparse_region(range.start, mapped_bytes, PageSize::Size4K)
                    .map_err(map_paging_err);
                pgtbl.finish();
                rollback?;
                return Err(error);
            }
            mapped_bytes += PAGE_SIZE_4K;
        }
        Ok(())
    }
}

impl BackendOps for LinearBackend {
    fn page_size(&self) -> PageSize {
        PageSize::Size4K
    }

    fn backing_info(&self) -> VmBackingInfo {
        VmBackingInfo::new(VmBackingKind::Linear, self.page_size())
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        if self.backing.is_some() {
            return self.map_owned_range(range, flags, pgtbl);
        }
        // External RAM/MMIO mappings keep their existing backing contract.
        let first_pa = self.pa(range.start)?;
        if range.size() > 0 {
            self.pa(range.end - 1)?;
        }
        pgtbl
            .map_region(
                range.start,
                |va| first_pa + (va - range.start),
                range.size(),
                flags,
                false,
            )
            .map_err(map_paging_err)
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        let pa_range = PhysAddrRange::from_start_size(self.pa(range.start)?, range.size());
        debug!("Linear::unmap: {range:?} -> {pa_range:?}");
        for vaddr in pages_in(range, PageSize::Size4K)? {
            match pgtbl.unmap(vaddr) {
                Ok(_) | Err(PagingError::NotMapped) => {}
                Err(err) => return Err(map_paging_err(err)),
            }
        }
        Ok(())
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        let addr = ctx.address().align_down(self.page_size());
        let expected = self.pa(addr)?;
        match pgtbl.query(addr) {
            Ok((paddr, page_flags, _)) => {
                if paddr != expected {
                    return Err(KError::BadAddress);
                }
                if !page_flags.contains(ctx.access_flags()) {
                    pgtbl.protect(addr, flags).map_err(map_paging_err)?;
                }
            }
            Err(PagingError::NotMapped) => {
                self.map_page(addr, flags, pgtbl)?;
            }
            Err(_) => return Err(KError::BadAddress),
        }
        Ok(FaultCompletion::from_populate((1, None)))
    }
}

impl VmRuntimeOps for LinearBackend {
    fn backing_info(&self) -> VmBackingInfo {
        BackendOps::backing_info(self)
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        BackendOps::map(self, range, flags, pgtbl)
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        BackendOps::unmap(self, range, pgtbl)
    }

    fn on_protect(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        BackendOps::on_protect(self, range, flags, pgtbl)
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        pgtbl: &mut PageTableMut,
    ) -> FaultCompletionResult {
        BackendOps::handle_fault(self, ctx, flags, pgtbl)
    }

    fn relocate_for_mremap(
        &self,
        _new_start: VirtAddr,
        _new_mm_id: u64,
        _aspace: &Arc<Mutex<MmSpace>>,
        _invalidate: Option<InvalidateHandle>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        Err(KError::OperationNotSupported)
    }

    fn clone_for_fork(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pgtbl: &mut PageTableMut,
        _new_pgtbl: &mut PageTableMut,
        _target: ForkCloneTarget<'_>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        Ok(Arc::new(self.clone()))
    }
}

unittest::path_mod_test!(
    owned_linear_unittest,
    "../../ut-tests/owned_linear_unittest.rs"
);
