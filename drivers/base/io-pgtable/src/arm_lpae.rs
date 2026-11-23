// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ARM long-descriptor (LPAE) page tables — SMMUv2 stage-1 format.
//!
//! Four-level 4 KiB-granule hierarchy covering up to 48 input bits, with
//! 1 GiB / 2 MiB block support and on-demand block splitting: mapping a
//! 4 KiB page through a level-1 block first splits the 1 GiB block into 2 MiB
//! blocks, then the hit 2 MiB block into pages, preserving the original block
//! attributes on the entries it fans out.
//!
//! The backend produces the SMMUv2 context-bank register surface
//! ([`ArmLpae::tcr_register`], [`ArmLpae::mair0_register`],
//! [`IoPageTable::root_table_address`]) so the SMMU driver programs hardware
//! without knowing the descriptor layout. Descriptor encodings follow the
//! ARM Architecture Reference Manual long-descriptor (VMSAv8-64) format.

use alloc::{sync::Arc, vec::Vec};

use crate::{Granule, IoPageTable, IommuProt, Iova, PgTableError, Stage1Config, TableAllocator};

/// Long-descriptor entry types in bits `[1:0]`.
const PTE_TYPE_FAULT: u64 = 0x0;
const PTE_TYPE_BLOCK: u64 = 0x1;
/// Table descriptors and level-3 page descriptors both use `0b11`.
const PTE_TYPE_TABLE: u64 = 0x3;

const PTE_ATTR_AF: u64 = 1 << 10;
const PTE_ATTR_NS: u64 = 1 << 5;
/// AP[2] (bit 6): privileged-only access when clear.
const PTE_AP_PRIVILEGED: u64 = 0;
/// AP[1] (bit 7): read-only when set.
const PTE_AP_RO: u64 = 1 << 7;
const PTE_XN: u64 = (1 << 54) | (1 << 53);
/// Table descriptor NS flag: subordinate lookups use non-secure PA space.
const PTE_TABLE_NS: u64 = 1 << 5;

/// MAIR attribute indices used in the `AttrIndex` PTE field.
const ATTR_INDEX_NORMAL_WB: u64 = 0;
const ATTR_INDEX_NORMAL_NC: u64 = 2;

/// MAIR0 encoding: index 0 normal write-back, index 1 device, index 2 normal
/// non-cacheable.
const MAIR0_VALUE: u64 = 0xff | (0x04 << 8) | (0x44 << 16);

const ENTRIES_PER_TABLE: usize = 512;
const LEVEL_BITS: u32 = 9;
/// Levels 0..3; level 3 holds page descriptors only.
const LEAF_LEVEL: u32 = 3;
/// Long-descriptor output address field: bits `[47:12]`.
const OA_MASK: u64 = 0x0000_ffff_ffff_f000;

/// Static configuration for one long-descriptor table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArmLpaeConfig {
    /// Input (IOVA) address width, 25..=48 bits.
    pub va_bits: u32,
    /// Output (PA) address width, 25..=48 bits.
    pub pa_bits: u32,
    /// Whether the SMMU translation walk is coherent with the CPU caches.
    pub coherent_walk: bool,
}

impl ArmLpaeConfig {
    /// SMMUv2 stage-1 defaults for the `arm,mmu-500` class: 48-bit IOVA and a
    /// non-coherent walk until the driver probes `IDR0.CTTW`.
    pub const SMMU_S1_DEFAULT: Self = Self {
        va_bits: 48,
        pa_bits: 44,
        coherent_walk: false,
    };
}

/// One allocated table page.
struct TablePage {
    ptr: *mut u64,
    physical: u64,
}

/// ARM long-descriptor page table backend.
///
/// Entries live in pages from [`TableAllocator`]; the hierarchy is owned
/// exclusively by the backend and freed on drop. All map/unmap calls must be
/// serialized by the owner (the IOMMU domain lock).
pub struct ArmLpae {
    config: ArmLpaeConfig,
    start_level: u32,
    allocator: Arc<dyn TableAllocator>,
    /// Every allocated table page, in allocation order; freed on drop.
    pages: Vec<TablePage>,
    root: TablePage,
}

// SAFETY: the backend exclusively owns its table pages; raw pointers are only
// dereferenced through `&mut self` operations, and `TableAllocator` is
// `Send + Sync`. Shared use (`translate`) reads through `&self` without
// concurrent writes, which the owning domain lock serializes.
unsafe impl Send for ArmLpae {}
// SAFETY: shared use is read-only (`translate`) and the owning domain lock
// serializes it against all mutable access.
unsafe impl Sync for ArmLpae {}

impl ArmLpae {
    /// Builds a 4 KiB-granule long-descriptor table.
    ///
    /// # Errors
    /// Returns [`PgTableError::UnsupportedConfig`] for widths outside the
    /// architectural range and [`PgTableError::NoMemory`] when the root table
    /// page cannot be allocated.
    pub fn new(
        config: ArmLpaeConfig,
        allocator: Arc<dyn TableAllocator>,
    ) -> Result<Self, PgTableError> {
        if !(25..=48).contains(&config.va_bits) || !(25..=48).contains(&config.pa_bits) {
            return Err(PgTableError::UnsupportedConfig);
        }
        let start_level = start_level_for(config.va_bits);
        let Some(root) = alloc_table_page(&*allocator) else {
            return Err(PgTableError::NoMemory);
        };
        Ok(Self {
            config,
            start_level,
            allocator,
            pages: Vec::new(),
            root,
        })
    }

    /// `CB_TCR` value programming this configuration into an SMMUv2 context
    /// bank: long-descriptor mode, 4 KiB granule, TTBR1 walks disabled and the
    /// walk attributes applied to TTBR0.
    pub fn tcr_register(&self) -> u64 {
        let mut tcr = 1u64 << 31; // EAE: long-descriptor format.
        tcr |= (64 - u64::from(self.config.va_bits)) & 0x3f; // T0SZ.
        tcr |= 0b00 << 14; // TG0 = 4 KiB.
        let share = if self.config.coherent_walk {
            0b11
        } else {
            0b00
        };
        tcr |= share << 12; // SH0: inner shareable for coherent walkers.
        // IRGN0/ORGN0 = 0b01: normal memory, write-back write-allocate.
        tcr |= 0b01 << 8;
        tcr |= 0b01 << 10;
        tcr |= 1 << 23; // EPD1: TTBR1 walks disabled (stage 1 uses TTBR0 only).
        tcr
    }

    /// `CB_S1_MAIR0` value matching the attribute indices used in PTEs.
    pub const fn mair0_register(&self) -> u64 {
        MAIR0_VALUE
    }

    /// Granule this backend maps with (always 4 KiB; larger sizes appear as
    /// block descriptors in [`IoPageTable::pgsize_bitmap`]).
    pub const fn granule(&self) -> Granule {
        Granule::Size4K
    }

    fn level_index(&self, level: u32, iova: Iova) -> usize {
        ((iova.as_u64() >> self.level_block_shift(level)) & 0x1ff) as usize
    }

    /// Byte shift of one descriptor at `level`: level 3 covers a page, each
    /// shallower level covers 512 times more.
    fn level_block_shift(&self, level: u32) -> u32 {
        page_shift() + LEVEL_BITS * (LEAF_LEVEL - level)
    }

    fn table_entry(&self, page: &TablePage, index: usize) -> u64 {
        // SAFETY: `index < ENTRIES_PER_TABLE` bounds `add` to the page.
        let slot = unsafe { page.ptr.add(index) };
        // SAFETY: `slot` addresses a live entry of this backend-owned page,
        // so the read cannot race a write.
        unsafe { *slot }
    }

    fn write_table_entry(&mut self, page: &TablePage, index: usize, value: u64) {
        // SAFETY: `index < ENTRIES_PER_TABLE` bounds `add` to the page.
        let slot = unsafe { page.ptr.add(index) };
        // SAFETY: entry writes are serialized by the owning domain lock, so
        // the store cannot race.
        unsafe {
            *slot = value;
        }
        // Descriptor writes must land in memory before any TLB maintenance the
        // hardware driver issues after the map/unmap returns.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    }

    /// Walks to the table containing `iova`'s descriptor at `level`, creating
    /// intermediate tables or splitting blocks on the way.
    fn walk_to_level(&mut self, iova: Iova, level: u32) -> Result<TablePage, PgTableError> {
        let mut current = clone_page(&self.root);
        for walk in self.start_level..level {
            let index = self.level_index(walk, iova);
            let entry = self.table_entry(&current, index);
            match entry & 0x3 {
                PTE_TYPE_TABLE => {
                    let physical = entry & OA_MASK;
                    let Some(found) = self
                        .pages
                        .iter()
                        .find(|page| page.physical == physical)
                        .map(clone_page)
                    else {
                        return Err(PgTableError::NoMemory);
                    };
                    current = found;
                }
                PTE_TYPE_FAULT => {
                    let created = self.alloc_table()?;
                    let descriptor = PTE_TYPE_TABLE | created.physical | PTE_TABLE_NS;
                    self.write_table_entry(&current, index, descriptor);
                    current = created;
                }
                _ => {
                    // A block descriptor below the requested level: split it
                    // into a subordinate table with identical attributes.
                    current = self.split_block(&current, index, walk)?;
                }
            }
        }
        Ok(current)
    }

    /// Replaces one block descriptor with a full subordinate table whose
    /// entries re-fan the block's output range at the next finer size.
    fn split_block(
        &mut self,
        parent: &TablePage,
        index: usize,
        block_level: u32,
    ) -> Result<TablePage, PgTableError> {
        let entry = self.table_entry(parent, index);
        let block_shift = self.level_block_shift(block_level);
        let sub_shift = block_shift - LEVEL_BITS;
        // Output base of the block: address bits above the block span.
        let block_base = entry & OA_MASK & !block_mask(block_shift);
        // Attributes survive the split: bits above the OA field (XN and
        // friends) plus the attribute bits below the block span (AF, NS,
        // AP, AttrIndex). The type bits are rebuilt per entry and the
        // fine-grained output bits come from `sub_base`.
        let attributes = (entry & !OA_MASK) | (entry & block_mask(block_shift) & !0x3);
        let created = self.alloc_table()?;
        let sub_type = descriptor_type(block_level + 1);
        for sub_index in 0..ENTRIES_PER_TABLE {
            let sub_base = block_base + (sub_index as u64) * (1 << sub_shift);
            let descriptor = attributes | sub_type | sub_base;
            self.write_table_entry(&created, sub_index, descriptor);
        }
        let descriptor = PTE_TYPE_TABLE | created.physical | PTE_TABLE_NS;
        self.write_table_entry(parent, index, descriptor);
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

impl Drop for ArmLpae {
    fn drop(&mut self) {
        let allocator = &*self.allocator;
        for page in &self.pages {
            // SAFETY: every recorded page is a live allocation from this
            // allocator and the backend owns the last reference.
            unsafe { allocator.free_table_page(page.ptr.cast::<u8>(), page.physical) };
        }
        // SAFETY: the root page is owned by this backend and dropped last.
        unsafe { allocator.free_table_page(self.root.ptr.cast::<u8>(), self.root.physical) };
    }
}

impl IoPageTable for ArmLpae {
    fn pgsize_bitmap(&self) -> u64 {
        // One descriptor size per level from the start level down to the
        // 4 KiB leaf: coarse blocks (>= 1 GiB) included.
        let mut bitmap = 0u64;
        for level in self.start_level..=LEAF_LEVEL {
            bitmap |= 1 << self.level_block_shift(level);
        }
        bitmap
    }

    fn stage1_config(&self) -> Option<Stage1Config> {
        Some(Stage1Config {
            tcr: self.tcr_register(),
            mair0: self.mair0_register(),
        })
    }

    fn root_table_address(&self) -> u64 {
        self.root.physical
    }

    fn map_pages(
        &mut self,
        iova: Iova,
        paddr: u64,
        pgsize: u64,
        count: usize,
        prot: IommuProt,
    ) -> Result<usize, PgTableError> {
        let level = leaf_level_for_pgsize(self, pgsize)?;
        if count == 0 {
            return Ok(0);
        }
        if !iova.as_u64().is_multiple_of(pgsize) || !paddr.is_multiple_of(pgsize) {
            return Err(PgTableError::Unaligned);
        }
        let span = pgsize
            .checked_mul(count as u64)
            .ok_or(PgTableError::Overflow)?;
        let last_byte = iova
            .as_u64()
            .checked_add(span - 1)
            .ok_or(PgTableError::Overflow)?;
        if last_byte >= 1 << self.config.va_bits {
            return Err(PgTableError::IovaOutOfRange);
        }
        let last_paddr = paddr.checked_add(span - 1).ok_or(PgTableError::Overflow)?;
        if last_paddr >= 1 << self.config.pa_bits {
            return Err(PgTableError::PhysOutOfRange);
        }

        let mut attributes = PTE_ATTR_AF | PTE_XN | PTE_ATTR_NS;
        attributes |= if prot.write {
            PTE_AP_PRIVILEGED
        } else {
            PTE_AP_RO
        };
        attributes |= (if prot.cacheable {
            ATTR_INDEX_NORMAL_WB
        } else {
            ATTR_INDEX_NORMAL_NC
        }) << 2;
        for step in 0..count {
            let entry_iova = iova.add(pgsize * step as u64);
            let entry_paddr = paddr + pgsize * step as u64;
            let table = self.walk_to_level(entry_iova, level)?;
            let index = self.level_index(level, entry_iova);
            let existing = self.table_entry(&table, index);
            if existing & 0x3 != PTE_TYPE_FAULT {
                let descriptor = attributes | descriptor_type(level) | entry_paddr;
                if existing != descriptor {
                    return Err(PgTableError::Conflict);
                }
                continue;
            }
            let descriptor = attributes | descriptor_type(level) | entry_paddr;
            self.write_table_entry(&table, index, descriptor);
        }
        Ok(count)
    }

    fn unmap_pages(
        &mut self,
        iova: Iova,
        pgsize: u64,
        count: usize,
    ) -> Result<usize, PgTableError> {
        let level = leaf_level_for_pgsize(self, pgsize)?;
        if count == 0 {
            return Ok(0);
        }
        let mut unmapped = 0usize;
        for step in 0..count {
            let entry_iova = iova.add(pgsize * step as u64);
            if entry_iova.as_u64() >= 1 << self.config.va_bits {
                break;
            }
            // An unmap may cover a coarser block descriptor (for example a
            // 2 MiB block created before a 4 KiB unmap of its head); split
            // anything in the way so the leaf entry exists.
            let table = self.walk_to_level(entry_iova, level)?;
            let index = self.level_index(level, entry_iova);
            if self.table_entry(&table, index) & 0x3 != PTE_TYPE_FAULT {
                self.write_table_entry(&table, index, PTE_TYPE_FAULT);
            }
            unmapped += 1;
        }
        Ok(unmapped)
    }

    fn translate(&self, iova: Iova) -> Option<u64> {
        let mut page = clone_page(&self.root);
        for level in self.start_level..=LEAF_LEVEL {
            let index = self.level_index(level, iova);
            let entry = self.table_entry(&page, index);
            match entry & 0x3 {
                PTE_TYPE_FAULT => return None,
                PTE_TYPE_TABLE if level < LEAF_LEVEL => {
                    let physical = entry & OA_MASK;
                    page = self
                        .pages
                        .iter()
                        .find(|candidate| candidate.physical == physical)
                        .map(clone_page)?;
                }
                _ => {
                    // Output base: address bits above the block offset; the
                    // attribute bits above the OA field are masked away.
                    let block_shift = self.level_block_shift(level);
                    let base = entry & OA_MASK & !block_mask(block_shift);
                    return Some(base | (iova.as_u64() & ((1 << block_shift) - 1)));
                }
            }
        }
        None
    }
}

/// Descriptor type bits for a leaf at `level`: blocks below level 3, page
/// descriptors (`0b11`) at level 3.
const fn descriptor_type(level: u32) -> u64 {
    if level == LEAF_LEVEL {
        PTE_TYPE_TABLE
    } else {
        PTE_TYPE_BLOCK
    }
}

const fn page_shift() -> u32 {
    Granule::Size4K.shift()
}

const fn block_mask(shift: u32) -> u64 {
    if shift >= 64 {
        u64::MAX
    } else {
        (1 << shift) - 1
    }
}

/// Start level for a 4 KiB-granule long-descriptor table covering `va_bits`.
const fn start_level_for(va_bits: u32) -> u32 {
    let span_bits = va_bits - page_shift();
    let levels_needed = span_bits.div_ceil(LEVEL_BITS);
    let clamped = if levels_needed > LEAF_LEVEL + 1 {
        LEAF_LEVEL + 1
    } else {
        levels_needed
    };
    LEAF_LEVEL + 1 - clamped
}

/// The level whose descriptor size matches `pgsize`.
fn leaf_level_for_pgsize(table: &ArmLpae, pgsize: u64) -> Result<u32, PgTableError> {
    if pgsize == 0 || !pgsize.is_power_of_two() {
        return Err(PgTableError::UnsupportedPageSize);
    }
    let wanted = pgsize.trailing_zeros();
    for level in table.start_level..=LEAF_LEVEL {
        if table.level_block_shift(level) == wanted {
            return Ok(level);
        }
    }
    Err(PgTableError::UnsupportedPageSize)
}

fn alloc_table_page(allocator: &dyn TableAllocator) -> Option<TablePage> {
    let (ptr, physical) = allocator.alloc_zeroed_table_page()?;
    Some(TablePage {
        // SAFETY: the allocator contract guarantees a 4 KiB-aligned page.
        ptr: ptr.cast::<u64>(),
        physical,
    })
}

fn clone_page(page: &TablePage) -> TablePage {
    TablePage {
        ptr: page.ptr,
        physical: page.physical,
    }
}
