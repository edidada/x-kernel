// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! General-purpose virtually-contiguous kernel memory allocator.

use alloc::vec::Vec;

use kaddr_layout::{IOMAP_VADDR, IOMAP_VSIZE, KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE};
use kalloc::global_allocator;
use kerrno::{KError, KResult};
use khal::{mem::v2p, paging::MappingFlags};
use memaddr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};

use crate::kernel_layout;

/// A virtually-contiguous allocation.
pub struct VmRegion {
    pub va: VirtAddr,
    pub size: usize,
}

/// VA range for vmalloc: starts above the iomap window so `unmap` does
/// not destroy direct-map PTEs (`KERNEL_ASPACE_BASE == linear_map_vaddr`
/// on all architectures).
fn vmalloc_range() -> VirtAddrRange {
    let after_fixed = IOMAP_VADDR + IOMAP_VSIZE;
    let remaining = KERNEL_ASPACE_SIZE.saturating_sub(after_fixed - KERNEL_ASPACE_BASE);
    VirtAddrRange::from_start_size(VirtAddr::from(after_fixed), remaining)
}

pub fn alloc(size: usize, guard_pages: usize) -> KResult<(VmRegion, Vec<usize>)> {
    let guard = guard_pages * PAGE_SIZE_4K;
    let usable = size - guard;
    let nr_pages = usable / PAGE_SIZE_4K;

    let mut aspace = kernel_layout().lock();
    let limit = vmalloc_range();
    let va = aspace
        .find_free_area(limit.start, size, limit, PAGE_SIZE_4K)
        .ok_or(KError::NoMemory)?;

    let mut pages = Vec::with_capacity(nr_pages);
    for i in 0..nr_pages {
        let page_va = va + guard + i * PAGE_SIZE_4K;
        let dm_va = global_allocator()
            .alloc_pages(1, PAGE_SIZE_4K, kalloc::UsageKind::VirtMem)
            .map_err(|_| KError::NoMemory)?;
        let pa = v2p(VirtAddr::from(dm_va));
        aspace
            .map_linear(
                page_va,
                pa,
                PAGE_SIZE_4K,
                MappingFlags::READ | MappingFlags::WRITE,
            )
            .map_err(|_| KError::NoMemory)?;
        pages.push(dm_va);
    }

    Ok((VmRegion { va, size }, pages))
}

pub fn free(region: VmRegion, guard_pages: usize, pages: Vec<usize>) {
    let guard = guard_pages * PAGE_SIZE_4K;
    let usable = region.size - guard;
    let usable_start = region.va + guard;

    let mut aspace = kernel_layout().lock();
    let _ = aspace.unmap(usable_start, usable);
    for dm_va in pages {
        global_allocator().dealloc_pages(dm_va, 1, kalloc::UsageKind::VirtMem);
    }
}
