// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The global kernel virtual address space.

use core::fmt;

use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize, PageTable, PagingError};
use memaddr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange, is_aligned_4k};

use crate::paging::map_paging_err;

/// The global kernel virtual address space.
///
/// Unlike [`UserMmSpace`], this type has no userspace VMA tree or process
/// lifetime state.  The page table is the single source of truth for kernel
/// mapping state: linear/kimage mappings are described by `kaddr_layout`
/// formulas, and dynamic windows (vmalloc/iomap) own their VA reservations
/// in `kernel_va::KernelVaAllocator`.  No parallel BTreeMap of mapping
/// metadata is kept; callers that need flags read them back from the PTE
/// via [`PageTable::query`], matching the Linux convention for kernel text
/// patching (`set_memory_rw` / `set_memory_ro`).
pub struct KernelAddressSpace {
    range: VirtAddrRange,
    pgtbl: PageTable,
}

impl KernelAddressSpace {
    /// Creates an empty kernel page table covering `[base, base + size)`.
    pub fn new_empty(base: VirtAddr, size: usize) -> KResult<Self> {
        let pgtbl = PageTable::try_new_kernel().map_err(|_| KError::NoMemory)?;
        let range = VirtAddrRange::try_from_start_size(base, size).ok_or(KError::InvalidInput)?;
        Ok(Self { range, pgtbl })
    }

    /// Explicitly named constructor retained for callers that distinguish the
    /// kernel page-table flavor at the call site.
    pub fn new_empty_kernel(base: VirtAddr, size: usize) -> KResult<Self> {
        Self::new_empty(base, size)
    }

    /// Returns the address-space base.
    pub const fn base(&self) -> VirtAddr {
        self.range.start
    }

    /// Returns the address-space end.
    pub const fn end(&self) -> VirtAddr {
        self.range.end
    }

    /// Returns the address-space size.
    pub fn size(&self) -> usize {
        self.range.size()
    }

    /// Returns the page table.
    pub const fn page_table(&self) -> &PageTable {
        &self.pgtbl
    }

    /// Returns the mutable page table for region-specific kernel managers.
    ///
    /// Callers using this escape hatch own the corresponding VA bookkeeping
    /// and must commit with `finish_no_flush()` followed by a kernel TLB
    /// shootdown outside `KERNEL_ASPACE`.  `vmalloc` is the production user;
    /// kernel TLB integration tests also use it for synthetic mappings.  Such
    /// callers must keep the `USER` permission clear.
    pub const fn page_table_mut(&mut self) -> &mut PageTable {
        &mut self.pgtbl
    }

    /// Returns the page-table root physical address.
    pub const fn page_table_root(&self) -> PhysAddr {
        self.pgtbl.root_paddr()
    }

    fn validate_region(&self, start: VirtAddr, size: usize) -> KResult {
        if size == 0
            || !self.range.contains_range(
                VirtAddrRange::try_from_start_size(start, size).ok_or(KError::InvalidInput)?,
            )
        {
            return Err(KError::NoMemory);
        }
        if !start.is_aligned_4k() || !is_aligned_4k(size) {
            return Err(KError::InvalidInput);
        }
        Ok(())
    }

    /// Installs a direct linear mapping without userspace VMA metadata.
    ///
    /// Kernel mappings reject the `USER` permission explicitly; user mappings
    /// belong to [`UserMmSpace`] and its VMA policy.
    ///
    /// The page-table transaction is committed with `finish_no_flush`; the
    /// caller must perform the kernel TLB shootdown after releasing its address
    /// space lock.  This ordering is required because shootdown IPIs can
    /// target CPUs which are waiting for the same lock with IRQs disabled.
    pub fn map_linear(
        &mut self,
        start_vaddr: VirtAddr,
        start_paddr: PhysAddr,
        size: usize,
        flags: MappingFlags,
    ) -> KResult {
        self.validate_region(start_vaddr, size)?;
        if flags.contains(MappingFlags::USER) {
            return Err(KError::PermissionDenied);
        }
        if !start_paddr.is_aligned_4k() || start_paddr.checked_add(size).is_none() {
            return Err(KError::InvalidInput);
        }
        let mut modify = self.pgtbl.modify();
        for offset in (0..size).step_by(PAGE_SIZE_4K) {
            let va = start_vaddr + offset;
            let pa = start_paddr + offset;
            if let Err(err) = modify.map(va, pa, PageSize::Size4K, flags) {
                for rollback in (0..offset).step_by(PAGE_SIZE_4K) {
                    let _ = modify.unmap(start_vaddr + rollback);
                }
                modify.finish_no_flush();
                return Err(map_paging_err(err));
            }
        }
        modify.finish_no_flush();
        Ok(())
    }

    /// Removes the PTEs for a kernel mapping range.
    ///
    /// The caller must perform the kernel TLB shootdown after releasing the
    /// address-space lock; the VA remains owned by the caller (allocator
    /// reservation or fixed mapping) until that teardown completes.
    pub fn unmap_kernel_range(&mut self, start: VirtAddr, size: usize) -> KResult {
        self.validate_region(start, size)?;
        let mut modify = self.pgtbl.modify();
        for vaddr in (0..size).step_by(PAGE_SIZE_4K) {
            match modify.unmap(start + vaddr) {
                Ok(_) | Err(PagingError::NotMapped) => {}
                Err(err) => {
                    modify.finish_no_flush();
                    return Err(map_paging_err(err));
                }
            }
        }
        modify.finish_no_flush();
        Ok(())
    }

    /// Changes flags for a kernel mapping range and commits without flushing.
    ///
    /// The `USER` permission is not accepted for global kernel mappings.
    ///
    /// The caller must release the global address-space lock and invoke the
    /// kernel TLB shootdown before code relies on the new permissions.
    pub fn protect(&mut self, start: VirtAddr, size: usize, flags: MappingFlags) -> KResult {
        self.validate_region(start, size)?;
        if flags.contains(MappingFlags::USER) {
            return Err(KError::PermissionDenied);
        }
        let mut modify = self.pgtbl.modify();
        if let Err(err) = modify.protect_region(start, size, flags) {
            modify.finish_no_flush();
            return Err(map_paging_err(err));
        }
        modify.finish_no_flush();
        Ok(())
    }
}

impl fmt::Debug for KernelAddressSpace {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("KernelAddressSpace")
            .field("va_range", &self.range)
            .field("page_table_root", &self.pgtbl.root_paddr())
            .finish()
    }
}
