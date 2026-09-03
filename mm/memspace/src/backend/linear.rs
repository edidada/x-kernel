// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linear mapping backend.
use alloc::sync::Arc;

use kerrno::KResult;
use khal::paging::{MappingFlags, PageSize, PageTableMut};
use ksync::Mutex;
use memaddr::{PhysAddr, PhysAddrRange, VirtAddr, VirtAddrRange};

use crate::{
    aspace::AddrSpace,
    backend::{Backend, BackendOps, map_paging_err},
};

/// Linear mapping backend.
///
/// The difference between virtual and physical addresses is constant modulo
/// `2^usize`. Physical address is `vaddr.wrapping_sub(offset)` where `offset`
/// is stored as the `isize` bit-pattern of that wrapping difference.
#[derive(Clone)]
pub struct LinearBackend {
    offset: isize,
}

impl LinearBackend {
    fn pa(&self, va: VirtAddr) -> PhysAddr {
        // Wrapping keeps high PAs (bit 63 set) representable. Signed
        // `as isize` subtraction overflows when VA and PA sit on opposite
        // halves of the address space.
        PhysAddr::from(va.as_usize().wrapping_sub(self.offset as usize))
    }
}

impl BackendOps for LinearBackend {
    fn page_size(&self) -> PageSize {
        PageSize::Size4K
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, pgtbl: &mut PageTableMut) -> KResult {
        let pa_range = PhysAddrRange::from_start_size(self.pa(range.start), range.size());
        debug!("Linear::map: {range:?} -> {pa_range:?} {flags:?}");
        pgtbl
            .map_region(range.start, |va| self.pa(va), range.size(), flags, false)
            .map_err(map_paging_err)?;
        Ok(())
    }

    fn unmap(&self, range: VirtAddrRange, pgtbl: &mut PageTableMut) -> KResult {
        let pa_range = PhysAddrRange::from_start_size(self.pa(range.start), range.size());
        debug!("Linear::unmap: {range:?} -> {pa_range:?}");
        pgtbl
            .unmap_region(range.start, range.size())
            .map_err(map_paging_err)?;
        Ok(())
    }

    fn clone_map(
        &self,
        _range: VirtAddrRange,
        _flags: MappingFlags,
        _old_pgtbl: &mut PageTableMut,
        _new_pgtbl: &mut PageTableMut,
        _new_aspace: &Arc<Mutex<AddrSpace>>,
    ) -> KResult<Backend> {
        Ok(Backend::Linear(self.clone()))
    }
}

impl Backend {
    /// Create a linear mapping backend with a fixed PA-VA offset.
    pub fn new_linear(offset: isize) -> Self {
        Self::Linear(LinearBackend { offset })
    }
}
