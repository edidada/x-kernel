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
    type MappingReference = kalloc::FrameRun<'static>;

    fn mapping_reference_paddr(reference: &Self::MappingReference) -> PhysAddr {
        reference.phys_addr()
    }

    fn mapping_reference_bytes(reference: &Self::MappingReference) -> usize {
        reference.frame_count() * memaddr::PAGE_SIZE_4K
    }

    fn try_clone_mapping_reference(
        reference: &Self::MappingReference,
    ) -> PagingResult<Self::MappingReference> {
        reference.try_clone_run().map_err(|_| PagingError::NoMemory)
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
    #[def_test]
    fn owned_replacement_keeps_old_backing_until_tlb_finish() {
        let _guard = NoPreemptIrqSave::new();
        let mut table = super::PageTable::try_new().unwrap();
        let allocator = global_allocator();
        let old = allocator.alloc_frame(kalloc::UsageKind::VirtMem).unwrap();
        let old_meta = allocator.frame_meta(old.pfn()).unwrap();
        let new = allocator
            .alloc_frame_run(1, PAGE_SIZE_4K, kalloc::UsageKind::VirtMem)
            .unwrap();
        let new_meta = allocator
            .frame_meta(new.phys_addr().as_usize() / PAGE_SIZE_4K)
            .unwrap();
        let address = memaddr::VirtAddr::from_usize(0x800000);
        let flags = super::MappingFlags::READ | super::MappingFlags::WRITE;
        let mut edit = table.modify();
        edit.map_owned(address, old, flags).unwrap();
        let snapshot = edit.query_entry(address).unwrap();
        edit.replace_owned_if_same(address, snapshot, new, flags)
            .unwrap();
        assert_eq!(old_meta.state(), FrameState::Shared { references: 1 });
        let rejected = allocator
            .alloc_frame_run(1, PAGE_SIZE_4K, kalloc::UsageKind::VirtMem)
            .unwrap();
        assert!(matches!(
            edit.replace_owned_if_same(address, snapshot, rejected, flags),
            Err(page_table::PteReplaceError::Changed { .. })
        ));
        assert_eq!(new_meta.state(), FrameState::Shared { references: 1 });
        edit.finish();
        assert_eq!(old_meta.state(), FrameState::Unshared);
        edit.unmap(address).unwrap();
        assert_eq!(new_meta.state(), FrameState::Shared { references: 1 });
        edit.finish();
        assert_eq!(new_meta.state(), FrameState::Unshared);
    }

    #[def_test]
    fn interior_huge_unmap_retires_every_backing_page_after_finish() {
        let _guard = NoPreemptIrqSave::new();
        let size = super::PageSize::Size2M;
        let allocator = global_allocator();
        let owner = allocator
            .alloc_frame_run(
                size as usize / PAGE_SIZE_4K,
                size as usize,
                kalloc::UsageKind::VirtMem,
            )
            .unwrap();
        let first = owner.phys_addr().as_usize() / PAGE_SIZE_4K;
        // Keep a separate observer hold: concurrent tests may reuse pages as
        // soon as they reach buddy, even with this CPU's IRQs disabled.
        let observer = owner.try_clone_run().unwrap();
        let mut table = super::PageTable::try_new().unwrap();
        let address = memaddr::VirtAddr::from_usize(0x1000000);
        let mut edit = table.modify();
        edit.map_owned_sized(address, owner, size, super::MappingFlags::READ)
            .unwrap();
        let (_, _, leaf_size) = edit.unmap(address + 77 * PAGE_SIZE_4K).unwrap();
        assert_eq!(leaf_size, size);
        for index in 0..size as usize / PAGE_SIZE_4K {
            assert_eq!(
                allocator.frame_meta(first + index).unwrap().state(),
                FrameState::Shared { references: 2 }
            );
        }
        edit.finish();
        for index in 0..size as usize / PAGE_SIZE_4K {
            assert_eq!(
                allocator.frame_meta(first + index).unwrap().state(),
                FrameState::Shared { references: 1 }
            );
        }
        drop(observer);
    }
}
