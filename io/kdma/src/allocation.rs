// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Coherent-page acquisition and reverse-order cleanup, before publication.

use core::alloc::Layout;

use alloc_engine::{AllocError, AllocResult};
use kalloc::{UsageKind, global_allocator};
use khal::mem::v2p;
use kiface_mm_api::{DmaMappingIf, DmaMappingMode};
use memaddr::{PAGE_SIZE_4K, va};

/// Internal operations shared by the real transaction and fault-injection tests.
pub(crate) trait AllocationOps {
    fn allocate(&mut self, pages: usize, align: usize) -> AllocResult<usize>;
    fn map_coherent(&mut self, address: usize, pages: usize) -> AllocResult<()>;
    fn prepare(&mut self, address: usize, pages: usize) -> AllocResult<()>;
    fn release(&mut self, address: usize, pages: usize) -> AllocResult<()>;
    fn restore(&mut self, address: usize, pages: usize) -> AllocResult<()>;
    fn free(&mut self, address: usize, pages: usize);
}

pub(crate) struct KernelAllocationOps;

impl AllocationOps for KernelAllocationOps {
    fn allocate(&mut self, pages: usize, align: usize) -> AllocResult<usize> {
        global_allocator().alloc_dma_pages(pages, align, UsageKind::Dma)
    }

    fn map_coherent(&mut self, address: usize, pages: usize) -> AllocResult<()> {
        DmaMappingIf::set_mode(va!(address), pages * PAGE_SIZE_4K, DmaMappingMode::Coherent)
            .map_err(|_| AllocError::NoMemory)
    }

    fn prepare(&mut self, address: usize, pages: usize) -> AllocResult<()> {
        kplat::dma::prepare(v2p(va!(address)).as_usize(), pages * PAGE_SIZE_4K)
            .map_err(|_| AllocError::NoMemory)
    }

    fn release(&mut self, address: usize, pages: usize) -> AllocResult<()> {
        kplat::dma::release(v2p(va!(address)).as_usize(), pages * PAGE_SIZE_4K)
            .map_err(|_| AllocError::NoMemory)
    }

    fn restore(&mut self, address: usize, pages: usize) -> AllocResult<()> {
        DmaMappingIf::set_mode(va!(address), pages * PAGE_SIZE_4K, DmaMappingMode::Normal)
            .map_err(|_| AllocError::NoMemory)
    }

    fn free(&mut self, address: usize, pages: usize) {
        global_allocator().dealloc_dma_pages(address, pages, UsageKind::Dma);
    }
}

pub(crate) fn allocate(layout: Layout, ops: &mut impl AllocationOps) -> AllocResult<usize> {
    if layout.size() == 0 {
        return Err(AllocError::InvalidInput);
    }
    let pages = layout.size().div_ceil(PAGE_SIZE_4K);
    let address = ops.allocate(pages, PAGE_SIZE_4K.max(layout.align()))?;
    if let Err(error) = ops.map_coherent(address, pages) {
        restore_and_free(address, pages, ops);
        return Err(error);
    }
    if let Err(error) = ops.prepare(address, pages) {
        release(address, pages, ops);
        return Err(error);
    }
    Ok(address)
}

/// Failed cleanup deliberately retains ownership outside the reusable page pool.
pub(crate) fn release(address: usize, pages: usize, ops: &mut impl AllocationOps) -> bool {
    if let Err(error) = ops.release(address, pages) {
        log::error!(
            "DMA cleanup: platform release failed; retaining {address:#x}, {pages} pages: \
             {error:?}"
        );
        return false;
    }
    restore_and_free(address, pages, ops)
}

fn restore_and_free(address: usize, pages: usize, ops: &mut impl AllocationOps) -> bool {
    if let Err(error) = ops.restore(address, pages) {
        log::error!(
            "DMA cleanup: mapping restore failed; retaining {address:#x}, {pages} pages: {error:?}"
        );
        return false;
    }
    ops.free(address, pages);
    true
}

unittest::path_mod_test!(tests, "../ut-tests/allocation.rs");
