// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Page table manipulation.

use kalloc::{UsageKind, global_allocator};
use memaddr::{PhysAddr, VirtAddr};
use page_table::PagingHandler;
#[doc(no_inline)]
pub use page_table::{
    PageSize, PagingFlags as MappingFlags, PtError as PagingError, PtResult as PagingResult,
};

use crate::mem::p2v;

/// Implementation of [`PagingHandler`], to provide physical memory manipulation
/// to the [page_table] crate.
pub struct PagingHandlerImpl;

impl PagingHandler for PagingHandlerImpl {
    type MappingReference = kalloc::FrameRef<'static>;

    fn mapping_reference_paddr(reference: &Self::MappingReference) -> PhysAddr {
        reference.phys_addr()
    }

    fn try_clone_mapping_reference(
        reference: &Self::MappingReference,
    ) -> PagingResult<Self::MappingReference> {
        reference.try_clone_ref().map_err(|_| PagingError::NoMemory)
    }

    fn alloc_frame() -> Option<PhysAddr> {
        assert!(
            kalloc::is_page_allocator_ready(),
            "page-table allocation requested before page allocator initialization"
        );
        global_allocator()
            .alloc_frame(UsageKind::PageTable)
            .map(kalloc::FrameRef::into_raw)
            .ok()
    }

    fn dealloc_frame(paddr: PhysAddr) {
        // SAFETY: page_table retires only frames returned by this handler,
        // exactly once after their table ownership and required translation
        // retirement end. Each allocation transferred one reference into the
        // raw PA; borrowed table entries are not deallocated by their borrower.
        drop(unsafe { global_allocator().frame_from_raw(paddr) });
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

#[cfg(unittest)]
unittest::path_mod_test!(pub mod tests_paging, "../ut-tests/paging.rs");

#[cfg(unittest)]
mod tests_frame_refs {
    use kalloc::{FrameState, global_allocator};
    use kspin::NoPreemptIrqSave;
    use memaddr::PAGE_SIZE_4K;
    use page_table::PagingHandler;
    use unittest::def_test;

    use super::PagingHandlerImpl;

    #[def_test]
    fn handler_transfers_one_reference_until_table_retirement() {
        // Local IRQ/preemption exclusion also keeps this base-page cache's
        // newly returned page from being reused before the final assertion.
        let _guard = NoPreemptIrqSave::new();
        let pa = PagingHandlerImpl::alloc_frame().unwrap();
        let meta = global_allocator()
            .frame_meta(pa.as_usize() / PAGE_SIZE_4K)
            .unwrap();
        assert_eq!(meta.state(), FrameState::Shared { references: 1 });
        // The fixture never installs this frame as an active hardware table,
        // so no translation retirement is needed before ending its ownership.
        PagingHandlerImpl::dealloc_frame(pa);
        assert_eq!(meta.state(), FrameState::Unshared);
    }
}
