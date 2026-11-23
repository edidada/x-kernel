// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hardware-independent IO page tables.
//!
//! This crate owns the [`IoPageTable`] contract and the two first-page
//! backends the IOMMU core consumes: [`arm_lpae`] (ARM long-descriptor,
//! SMMUv2 stage 1) and [`rockchip_v2`] (Rockchip two-level IOMMU).
//!
//! A backend translates IOVA ranges to physical addresses through its own
//! in-memory table hierarchy and reports the register programming surface the
//! owning hardware needs ([`ArmLpae::tcr_register`],
//! [`ArmLpae::mair0_register`], [`IoPageTable::root_table_address`]). It never
//! touches devices: TLB invalidation stays with the hardware driver, which is
//! the only component that knows the register file.
//!
//! # Table memory
//!
//! Page tables must live in memory the IOMMU's translation walk can read, so
//! backends take a [`TableAllocator`] that returns physical addresses instead
//! of allocating raw pages themselves. Host kernels back it with their DMA
//! coherent allocator; unit tests use a linear fake.
//!
//! # Unmapping
//!
//! `unmap_pages` clears PTEs but keeps intermediate tables allocated so a
//! later map of the same range does not re-fault the walk. IOMMU translation
//! windows live for the domain lifetime; the host releases the whole
//! hierarchy with the domain by dropping the backend, which frees every table
//! through the allocator.

#![no_std]

extern crate alloc;

use core::fmt;

pub mod arm_lpae;
pub mod rockchip_v2;

pub use arm_lpae::{ArmLpae, ArmLpaeConfig};
pub use rockchip_v2::RockchipV2;

/// An IOMMU virtual address (the address a device issues on its bus).
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Iova(u64);

impl Iova {
    /// Wraps a raw IOVA value.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw value.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Adds a byte offset without overflow checks (callers bound the window).
    pub const fn add(self, offset: u64) -> Self {
        Self(self.0 + offset)
    }
}

/// Access permissions programmed into a mapping.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IommuProt {
    /// Device may read.
    pub read: bool,
    /// Device may write.
    pub write: bool,
    /// Mapping is cacheable (inner write-back on ARM backends).
    pub cacheable: bool,
}

impl IommuProt {
    /// Read-write cacheable mapping (the common DMA case).
    pub const RW_CACHEABLE: Self = Self {
        read: true,
        write: true,
        cacheable: true,
    };
}

/// Physical page-table memory provider.
///
/// Backends program physical addresses into descriptor entries, so the
/// allocator must return memory whose device-visible physical address is
/// known: host kernels use their DMA coherent allocator, unit tests use a
/// fake linear mapping.
pub trait TableAllocator: Send + Sync {
    /// Allocates one zeroed table page (4 KiB) and returns its CPU pointer and
    /// physical address.
    ///
    /// Errors are reported as `None`; backends map that to their own error
    /// type. The page must be 4 KiB aligned in both spaces.
    fn alloc_zeroed_table_page(&self) -> Option<(*mut u8, u64)>;

    /// Frees a page previously returned by
    /// [`TableAllocator::alloc_zeroed_table_page`].
    ///
    /// # Safety
    ///
    /// `(ptr, physical)` must be a live allocation from this allocator that is
    /// freed exactly once and is no longer referenced by any table.
    unsafe fn free_table_page(&self, ptr: *mut u8, physical: u64);
}

/// Page granule supported by a backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Granule {
    /// 4 KiB pages.
    Size4K,
    /// 16 KiB pages.
    Size16K,
    /// 64 KiB pages.
    Size64K,
}

impl Granule {
    /// Page size in bytes.
    pub const fn bytes(self) -> u64 {
        match self {
            Granule::Size4K => 4096,
            Granule::Size16K => 16 * 1024,
            Granule::Size64K => 64 * 1024,
        }
    }

    /// log2 of the page size.
    pub const fn shift(self) -> u32 {
        match self {
            Granule::Size4K => 12,
            Granule::Size16K => 14,
            Granule::Size64K => 16,
        }
    }
}

/// Why a map or unmap request failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PgTableError {
    /// The table allocator could not supply an intermediate table page.
    NoMemory,
    /// The IOVA range does not fit the configured translation window.
    IovaOutOfRange,
    /// The physical address does not fit the configured output window.
    PhysOutOfRange,
    /// `pgsize` is not in the backend's supported granularity bitmap.
    UnsupportedPageSize,
    /// The range is not aligned to the requested page size.
    Unaligned,
    /// The range overlaps an existing mapping with different attributes.
    Conflict,
    /// The caller's length arguments overflow the address space.
    Overflow,
    /// The requested configuration exceeds the backend's fixed constraints.
    UnsupportedConfig,
}

impl fmt::Display for PgTableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            PgTableError::NoMemory => "no memory for table pages",
            PgTableError::IovaOutOfRange => "iova outside the translation window",
            PgTableError::PhysOutOfRange => "physical address outside the output window",
            PgTableError::UnsupportedPageSize => "page size not in the backend bitmap",
            PgTableError::Unaligned => "range not aligned to the page size",
            PgTableError::Conflict => "range overlaps a different existing mapping",
            PgTableError::Overflow => "address arithmetic overflow",
            PgTableError::UnsupportedConfig => "configuration exceeds backend constraints",
        };
        formatter.write_str(text)
    }
}

/// Stage-1 context configuration a backend exposes to SMMU hardware.
///
/// Backends without a stage-1 register surface (for example the Rockchip
/// two-level table) return `None` from
/// [`IoPageTable::stage1_config`]; their hardware programs the root address
/// alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stage1Config {
    /// `CB_TCR` value for an SMMUv2 context bank (long-descriptor mode).
    pub tcr: u64,
    /// `CB_S1_MAIR0` value matching the PTE attribute indices.
    pub mair0: u64,
}

/// Hardware-independent IO page table backend.
///
/// Implementations own their table hierarchy. All operations are serialized by
/// the owning [`crate`]-external caller (the IOMMU domain lock); the trait is
/// `Send + Sync` so a domain can hand the backend to its hardware driver for
/// root-address programming.
///
/// `map_pages` maps `count` consecutive pages of one granularity and returns
/// the number of pages actually mapped: a backend may stop early when an
/// intermediate table allocation fails, and the caller must then unmap the
/// partially built range. `unmap_pages` returns the number of pages it
/// cleared; entries already absent count as unmapped.
pub trait IoPageTable: Send + Sync {
    /// Bitmask of page sizes this backend maps, in bytes.
    fn pgsize_bitmap(&self) -> u64;

    /// Physical address of the root table, to be programmed into the hardware
    /// (TTBR0 for SMMUv2 stage 1, DTE base for Rockchip).
    fn root_table_address(&self) -> u64;

    /// Maps `count` pages starting at `iova` to consecutive frames starting at
    /// `paddr`.
    ///
    /// # Errors
    /// Returns the first validation error before touching tables, or
    /// [`PgTableError::NoMemory`] when an allocation fails mid-walk after
    /// returning how many pages were placed.
    fn map_pages(
        &mut self,
        iova: Iova,
        paddr: u64,
        pgsize: u64,
        count: usize,
        prot: IommuProt,
    ) -> Result<usize, PgTableError>;

    /// Clears `count` pages starting at `iova` and returns how many entries
    /// were removed.
    ///
    /// # Errors
    /// Returns validation errors; clearing an empty entry is not an error.
    fn unmap_pages(&mut self, iova: Iova, pgsize: u64, count: usize)
    -> Result<usize, PgTableError>;

    /// Reads back the physical address one IOVA translates to, for tests and
    /// fault diagnostics.
    fn translate(&self, iova: Iova) -> Option<u64>;

    /// Stage-1 register surface for SMMU-style hardware, when the backend
    /// has one.
    fn stage1_config(&self) -> Option<Stage1Config> {
        None
    }
}

unittest::path_mod_test!(tests, "../ut-tests/pgtable.rs");
