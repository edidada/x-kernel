// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Rockchip two-level IOMMU page tables (v2 format, RK3568/RK3588).
//!
//! The Rockchip IOMMU walks a 2-level hierarchy: a Directory Table of 1024
//! 4-byte DTEs, each pointing at one Page Table of 1024 4-byte PTEs, so one
//! table set covers 4 GiB of IOVA in 4 KiB pages:
//!
//! ```text
//!               DT
//! DTE base -> +------+
//!             | DTE | -> PT
//!             +------+   +------+
//!             |      |   | PTE | -> 4 KiB page
//!             +------+   +------+
//! ```
//!
//! IOVA layout: `[31:22] DTE index | [21:12] PTE index | [11:0] page offset`.
//! The v2 descriptor format stretches the page-table and page addresses to 40
//! bits: bits `[35:32]` and `[39:36]` fold into DTE/PTE bits `[11:8]` and
//! `[7:4]` respectively. This backend always emits v2 descriptors; RK3568 and
//! RK3588 both decode them.

use alloc::{sync::Arc, vec::Vec};

use crate::{IoPageTable, IommuProt, Iova, PgTableError, TableAllocator};

/// Directory and page tables both hold 1024 4-byte entries (one 4 KiB page).
const ENTRIES_PER_TABLE: usize = 1024;

const DTE_PT_VALID: u32 = 1;
/// v2 DTE: PT address bits `[31:4]` carry PA `[31:4]`, `[11:8]` carry PA
/// `[35:32]`, `[7:4]` carry PA `[39:36]`.
const PTE_PAGE_VALID: u32 = 1;
const PTE_PAGE_READABLE: u32 = 1 << 1;
const PTE_PAGE_WRITABLE: u32 = 1 << 2;

/// Page size: every valid mapping is 4 KiB.
const SPAGE_SIZE: u64 = 4096;
/// One Page Table covers `1024 * 4 KiB`.
const PT_SPAN: u64 = ENTRIES_PER_TABLE as u64 * SPAGE_SIZE;
/// The whole hierarchy covers `1024 * 4 MiB` = 4 GiB of IOVA.
const IOVA_WINDOW: u64 = ENTRIES_PER_TABLE as u64 * PT_SPAN;

/// One allocated table page (Directory Table or one Page Table).
struct TablePage {
    ptr: *mut u32,
    physical: u64,
}

/// Rockchip v2 two-level page table.
///
/// Tables come from [`TableAllocator`]; the Directory Table is allocated at
/// construction and Page Tables on demand when a DTE is first hit. Unmapping
/// keeps Page Tables allocated for reuse; dropping the backend releases the
/// whole hierarchy.
pub struct RockchipV2 {
    allocator: Arc<dyn TableAllocator>,
    pages: Vec<TablePage>,
    directory: TablePage,
}

// SAFETY: the backend exclusively owns its table pages; raw pointers are only
// dereferenced through `&mut self` operations, and `TableAllocator` is
// `Send + Sync`. Shared reads happen through `translate` under the domain lock.
unsafe impl Send for RockchipV2 {}
// SAFETY: shared use is read-only (`translate`) and the owning domain lock
// serializes it against all mutable access.
unsafe impl Sync for RockchipV2 {}

impl RockchipV2 {
    /// Allocates the Directory Table.
    ///
    /// # Errors
    /// Returns [`PgTableError::NoMemory`] when the directory page cannot be
    /// allocated.
    pub fn new(allocator: Arc<dyn TableAllocator>) -> Result<Self, PgTableError> {
        let Some(directory) = alloc_table_page(&*allocator) else {
            return Err(PgTableError::NoMemory);
        };
        Ok(Self {
            allocator,
            pages: Vec::new(),
            directory,
        })
    }

    fn dte_index(&self, iova: Iova) -> usize {
        ((iova.as_u64() >> 22) & 0x3ff) as usize
    }

    fn pte_index(&self, iova: Iova) -> usize {
        ((iova.as_u64() >> 12) & 0x3ff) as usize
    }

    fn entry(&self, page: &TablePage, index: usize) -> u32 {
        // SAFETY: `index < ENTRIES_PER_TABLE` bounds `add` to the page.
        let slot = unsafe { page.ptr.add(index) };
        // SAFETY: `slot` addresses a live entry of this backend-owned page,
        // so the read cannot race a write.
        unsafe { *slot }
    }

    fn write_entry(&mut self, page: &TablePage, index: usize, value: u32) {
        // SAFETY: `index < ENTRIES_PER_TABLE` bounds `add` to the page.
        let slot = unsafe { page.ptr.add(index) };
        // SAFETY: writes are serialized by the owning domain lock, so the
        // store cannot race.
        unsafe {
            *slot = value;
        }
        // The hardware walk reads tables from memory; make descriptor writes
        // visible before the driver's ZAP_CACHE / TLB maintenance.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    }

    /// Returns the Page Table for `dte_index`, allocating and linking it on
    /// first touch.
    fn page_table_for(&mut self, dte_index: usize) -> Result<TablePage, PgTableError> {
        let dte = self.entry(&self.directory, dte_index);
        if dte & DTE_PT_VALID != 0 {
            let physical = dte_v2_address(dte);
            let Some(found) = self
                .pages
                .iter()
                .find(|page| page.physical == physical)
                .map(clone_page)
            else {
                return Err(PgTableError::NoMemory);
            };
            return Ok(found);
        }
        let created = self.alloc_table()?;
        let descriptor = mk_dte_v2(created.physical);
        let directory = clone_page(&self.directory);
        self.write_entry(&directory, dte_index, descriptor);
        Ok(created)
    }

    fn alloc_table(&mut self) -> Result<TablePage, PgTableError> {
        let Some(page) = alloc_table_page(&*self.allocator) else {
            return Err(PgTableError::NoMemory);
        };
        self.pages.push(clone_page(&page));
        Ok(page)
    }
}

impl Drop for RockchipV2 {
    fn drop(&mut self) {
        let allocator = &*self.allocator;
        for page in &self.pages {
            // SAFETY: every recorded page is a live allocation from this
            // allocator and the backend owns the last reference.
            unsafe { allocator.free_table_page(page.ptr.cast::<u8>(), page.physical) };
        }
        // SAFETY: the directory is owned by this backend and dropped last.
        unsafe {
            allocator.free_table_page(self.directory.ptr.cast::<u8>(), self.directory.physical)
        };
    }
}

impl IoPageTable for RockchipV2 {
    fn pgsize_bitmap(&self) -> u64 {
        SPAGE_SIZE
    }

    fn root_table_address(&self) -> u64 {
        self.directory.physical
    }

    fn map_pages(
        &mut self,
        iova: Iova,
        paddr: u64,
        pgsize: u64,
        count: usize,
        prot: IommuProt,
    ) -> Result<usize, PgTableError> {
        if pgsize != SPAGE_SIZE {
            return Err(PgTableError::UnsupportedPageSize);
        }
        if count == 0 {
            return Ok(0);
        }
        if !iova.as_u64().is_multiple_of(SPAGE_SIZE) || !paddr.is_multiple_of(SPAGE_SIZE) {
            return Err(PgTableError::Unaligned);
        }
        let Some(span) = pgsize.checked_mul(count as u64) else {
            return Err(PgTableError::Overflow);
        };
        let Some(last_byte) = iova.as_u64().checked_add(span - 1) else {
            return Err(PgTableError::Overflow);
        };
        if last_byte >= IOVA_WINDOW {
            return Err(PgTableError::IovaOutOfRange);
        }
        // The v2 descriptor format carries 40-bit output addresses.
        let last_paddr = paddr.checked_add(span - 1).ok_or(PgTableError::Overflow)?;
        if last_paddr >= 1 << 40 {
            return Err(PgTableError::PhysOutOfRange);
        }

        let mut flags = PTE_PAGE_VALID;
        if prot.read {
            flags |= PTE_PAGE_READABLE;
        }
        if prot.write {
            flags |= PTE_PAGE_WRITABLE;
        }

        for step in 0..count {
            let entry_iova = iova.add(SPAGE_SIZE * step as u64);
            let entry_paddr = paddr + SPAGE_SIZE * step as u64;
            let page_table = self.page_table_for(self.dte_index(entry_iova))?;
            let index = self.pte_index(entry_iova);
            let existing = self.entry(&page_table, index);
            if existing & PTE_PAGE_VALID != 0 {
                let descriptor = mk_pte_v2(entry_paddr, flags);
                if existing != descriptor {
                    return Err(PgTableError::Conflict);
                }
                continue;
            }
            self.write_entry(&page_table, index, mk_pte_v2(entry_paddr, flags));
        }
        Ok(count)
    }

    fn unmap_pages(
        &mut self,
        iova: Iova,
        pgsize: u64,
        count: usize,
    ) -> Result<usize, PgTableError> {
        if pgsize != SPAGE_SIZE {
            return Err(PgTableError::UnsupportedPageSize);
        }
        if count == 0 {
            return Ok(0);
        }
        let mut unmapped = 0usize;
        for step in 0..count {
            let entry_iova = iova.add(SPAGE_SIZE * step as u64);
            if entry_iova.as_u64() >= IOVA_WINDOW {
                break;
            }
            let page_table = self.page_table_for(self.dte_index(entry_iova))?;
            let index = self.pte_index(entry_iova);
            if self.entry(&page_table, index) & PTE_PAGE_VALID != 0 {
                self.write_entry(&page_table, index, 0);
            }
            unmapped += 1;
        }
        Ok(unmapped)
    }

    fn translate(&self, iova: Iova) -> Option<u64> {
        let dte = self.entry(&self.directory, self.dte_index(iova));
        if dte & DTE_PT_VALID == 0 {
            return None;
        }
        let physical = dte_v2_address(dte);
        let page_table = self
            .pages
            .iter()
            .find(|page| page.physical == physical)
            .map(clone_page)?;
        let pte = self.entry(&page_table, self.pte_index(iova));
        if pte & PTE_PAGE_VALID == 0 {
            return None;
        }
        Some(pte_v2_address(pte) | (iova.as_u64() & 0xfff))
    }
}

/// Encodes a 40-bit Page Table address into the v2 DTE layout:
/// bits `[35:32]` fold into DTE `[11:8]`, bits `[39:36]` into DTE `[7:4]`.
fn mk_dte_v2(pt_physical: u64) -> u32 {
    let high = (((pt_physical >> 32) & 0xf) << 8) | (((pt_physical >> 36) & 0xf) << 4);
    ((pt_physical as u32) & 0xffff_f000) | ((high & 0xff0) as u32) | DTE_PT_VALID
}

/// Decodes a v2 DTE into its 40-bit Page Table address.
fn dte_v2_address(dte: u32) -> u64 {
    let low = u64::from(dte & 0xffff_f000);
    let hi32 = u64::from((dte >> 8) & 0xf) << 32;
    let hi36 = u64::from((dte >> 4) & 0xf) << 36;
    low | hi32 | hi36
}

/// Encodes a 40-bit page address plus permission flags into a v2 PTE.
fn mk_pte_v2(page_physical: u64, flags: u32) -> u32 {
    let high = (((page_physical >> 32) & 0xf) << 8) | (((page_physical >> 36) & 0xf) << 4);
    ((page_physical as u32) & 0xffff_f000) | ((high & 0xff0) as u32) | flags
}

/// Decodes a v2 PTE into its 40-bit page address.
fn pte_v2_address(pte: u32) -> u64 {
    dte_v2_address(pte)
}

fn alloc_table_page(allocator: &dyn TableAllocator) -> Option<TablePage> {
    let (ptr, physical) = allocator.alloc_zeroed_table_page()?;
    Some(TablePage {
        // SAFETY: the allocator contract guarantees a 4 KiB-aligned page.
        ptr: ptr.cast::<u32>(),
        physical,
    })
}

fn clone_page(page: &TablePage) -> TablePage {
    TablePage {
        ptr: page.ptr,
        physical: page.physical,
    }
}
