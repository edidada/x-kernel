// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Page table manipulation.

use kalloc::{UsageKind, global_allocator};
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};
use page_table::PagingHandler;
#[doc(no_inline)]
pub use page_table::{
    PageSize, PagingFlags as MappingFlags, PtError as PagingError, PtResult as PagingResult,
};

use crate::mem::{p2v, v2p};

/// Implementation of [`PagingHandler`], to provide physical memory manipulation
/// to the [page_table] crate.
pub struct PagingHandlerImpl;

impl PagingHandler for PagingHandlerImpl {
    fn alloc_frame() -> Option<PhysAddr> {
        assert!(
            kalloc::is_page_allocator_ready(),
            "page-table allocation requested before page allocator initialization"
        );
        global_allocator()
            .alloc_pages(1, PAGE_SIZE_4K, UsageKind::PageTable)
            .map(|vaddr| v2p(vaddr.into()))
            .ok()
    }

    fn dealloc_frame(paddr: PhysAddr) {
        global_allocator().dealloc_pages(p2v(paddr).as_usize(), 1, UsageKind::PageTable);
    }

    #[inline]
    fn p2v(paddr: PhysAddr) -> VirtAddr {
        p2v(paddr)
    }
}

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        /// The architecture-specific page table.
        pub type PageTable = page_table::x86_64::X64PageTable<PagingHandlerImpl>;
        pub type PageTableMut<'a> = page_table::x86_64::X64PageTableMut<'a, PagingHandlerImpl>;
    } else if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
        /// The architecture-specific page table.
        pub type PageTable = page_table::riscv::Sv39PageTable<PagingHandlerImpl>;
        pub type PageTableMut<'a> = page_table::riscv::Sv39PageTableMut<'a, PagingHandlerImpl>;
    } else if #[cfg(target_arch = "aarch64")]{
        /// The architecture-specific page table.
        pub type PageTable = page_table::aarch64::A64PageTable<PagingHandlerImpl>;
        pub type PageTableMut<'a> = page_table::aarch64::A64PageTableMut<'a, PagingHandlerImpl>;
    } else if #[cfg(target_arch = "loongarch64")] {
        /// The architecture-specific page table.
        pub type PageTable = page_table::loongarch64::LA64PageTable<PagingHandlerImpl>;
        pub type PageTableMut<'a> = page_table::loongarch64::LA64PageTableMut<'a, PagingHandlerImpl>;
    }
}

/// Unmap a single 4 KiB leaf from the live kernel page table.
///
/// Returns `None` if `va` is not mapped as a 4 KiB leaf.
///
/// # Safety
/// The caller must ensure exclusive access to the kernel page table
/// (no concurrent modifications from other CPUs).
#[inline]
pub unsafe fn unmap_kernel_leaf_4k(
    root: PhysAddr,
    va: VirtAddr,
) -> Option<(PhysAddr, MappingFlags)> {
    // The caller of this `unsafe fn` guarantees exclusive access to the kernel
    // page table, satisfying the precondition of the underlying generic.
    cfg_if::cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            // SAFETY: caller guarantees exclusive page-table access.
            unsafe { page_table::unmap_kernel_leaf_4k::<
                page_table::x86_64::X64PagingMetaData,
                page_table::x86_64::X64PageEntry,
                PagingHandlerImpl,
            >(root, va) }
        } else if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
            // SAFETY: caller guarantees exclusive page-table access.
            unsafe { page_table::unmap_kernel_leaf_4k::<
                page_table::riscv::Sv39MetaData<VirtAddr>,
                page_table::riscv::Rv64PageEntry,
                PagingHandlerImpl,
            >(root, va) }
        } else if #[cfg(target_arch = "aarch64")] {
            // SAFETY: caller guarantees exclusive page-table access.
            unsafe { page_table::unmap_kernel_leaf_4k::<
                page_table::aarch64::A64PagingMetaData,
                page_table::aarch64::A64PageEntry,
                PagingHandlerImpl,
            >(root, va) }
        } else if #[cfg(target_arch = "loongarch64")] {
            // SAFETY: caller guarantees exclusive page-table access.
            unsafe { page_table::unmap_kernel_leaf_4k::<
                page_table::loongarch64::LA64MetaData,
                page_table::loongarch64::La64PageEntry,
                PagingHandlerImpl,
            >(root, va) }
        } else {
            compile_error!("unsupported target_arch for kernel guard page");
        }
    }
}

/// Map a single 4 KiB leaf into the live kernel page table.
///
/// This is the inverse of [`unmap_kernel_leaf_4k`].
///
/// # Errors
///
/// Returns [`PagingError`] if the map fails.
///
/// # Safety
/// The caller must ensure exclusive access to the kernel page table.
#[inline]
pub unsafe fn map_kernel_leaf_4k(
    root: PhysAddr,
    va: VirtAddr,
    phys: PhysAddr,
    flags: MappingFlags,
) -> PagingResult<()> {
    // The caller of this `unsafe fn` guarantees exclusive access to the kernel
    // page table, satisfying the precondition of the underlying generic.
    cfg_if::cfg_if! {
        if #[cfg(target_arch = "x86_64")] {
            // SAFETY: caller guarantees exclusive page-table access.
            unsafe { page_table::map_kernel_leaf_4k::<
                page_table::x86_64::X64PagingMetaData,
                page_table::x86_64::X64PageEntry,
                PagingHandlerImpl,
            >(root, va, phys, flags) }
        } else if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
            // SAFETY: caller guarantees exclusive page-table access.
            unsafe { page_table::map_kernel_leaf_4k::<
                page_table::riscv::Sv39MetaData<VirtAddr>,
                page_table::riscv::Rv64PageEntry,
                PagingHandlerImpl,
            >(root, va, phys, flags) }
        } else if #[cfg(target_arch = "aarch64")] {
            // SAFETY: caller guarantees exclusive page-table access.
            unsafe { page_table::map_kernel_leaf_4k::<
                page_table::aarch64::A64PagingMetaData,
                page_table::aarch64::A64PageEntry,
                PagingHandlerImpl,
            >(root, va, phys, flags) }
        } else if #[cfg(target_arch = "loongarch64")] {
            // SAFETY: caller guarantees exclusive page-table access.
            unsafe { page_table::map_kernel_leaf_4k::<
                page_table::loongarch64::LA64MetaData,
                page_table::loongarch64::La64PageEntry,
                PagingHandlerImpl,
            >(root, va, phys, flags) }
        } else {
            compile_error!("unsupported target_arch for kernel guard page");
        }
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_paging {
    use unittest::def_test;

    use super::MappingFlags;

    #[def_test]
    fn test_mapping_flags_combine() {
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        assert!(flags.contains(MappingFlags::READ));
        assert!(flags.contains(MappingFlags::WRITE));
    }

    #[def_test]
    fn test_mapping_flags_empty() {
        let flags = MappingFlags::empty();
        assert!(flags.is_empty());
    }
}
