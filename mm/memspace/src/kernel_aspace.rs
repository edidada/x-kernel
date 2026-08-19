// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The global kernel virtual address space.

use alloc::collections::BTreeMap;
use core::fmt;

use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize, PageTable, PagingError};
use memaddr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr, VirtAddrRange, is_aligned_4k};

use crate::paging::map_paging_err;

/// Metadata for one mapping owned by the kernel address space.
///
/// Kernel mappings deliberately carry only the information needed by kernel
/// services (range and current PTE flags).  They do not carry process VMA
/// fields such as backing objects, inheritance or fault policy.
#[derive(Debug, Clone, Copy)]
pub struct KernelMapping {
    range: VirtAddrRange,
    flags: MappingFlags,
}

impl KernelMapping {
    /// Start of the mapping.
    pub const fn start(&self) -> VirtAddr {
        self.range.start
    }

    /// End of the mapping (exclusive).
    pub const fn end(&self) -> VirtAddr {
        self.range.end
    }

    /// Size of the mapping in bytes.
    pub fn size(&self) -> usize {
        self.range.size()
    }

    /// Current page-table flags for the mapping.
    pub const fn flags(&self) -> MappingFlags {
        self.flags
    }
}

/// Non-overlapping interval set used by [`KernelAddressSpace`].
///
/// This is intentionally smaller than [`VmAreaSet`].  Kernel mappings are
/// installed and removed explicitly; they do not participate in mmap,
/// object invalidation, COW or page-fault dispatch.
#[derive(Default)]
struct KernelMappingSet {
    areas: BTreeMap<VirtAddr, KernelMapping>,
}

impl KernelMappingSet {
    fn find(&self, addr: VirtAddr) -> Option<&KernelMapping> {
        let mapping = self.areas.range(..=addr).next_back()?.1;
        mapping.range.contains(addr).then_some(mapping)
    }

    fn overlaps(&self, range: VirtAddrRange) -> bool {
        self.areas
            .range(..range.end)
            .next_back()
            .is_some_and(|(_, mapping)| mapping.end() > range.start)
            || self
                .areas
                .range(range.start..)
                .next()
                .is_some_and(|(_, mapping)| mapping.start() < range.end)
    }

    /// Returns whether every byte in `range` belongs to a recorded mapping.
    ///
    /// A range may cross adjacent mappings (for example after a protection
    /// split), but a hole must never be treated as owned by this set.
    fn covers(&self, range: VirtAddrRange) -> bool {
        let mut cursor = range.start;
        while cursor < range.end {
            let Some(mapping) = self.find(cursor) else {
                return false;
            };
            if mapping.end() <= cursor {
                return false;
            }
            cursor = mapping.end();
        }
        true
    }

    fn find_free_area(
        &self,
        hint: VirtAddr,
        size: usize,
        limit: VirtAddrRange,
        align: usize,
    ) -> Option<VirtAddr> {
        if size == 0 || align == 0 || !size.is_multiple_of(align) {
            return None;
        }
        let mut last_end = hint.max(limit.start).align_up(align);
        if let Some((_, mapping)) = self.areas.range(..last_end).last() {
            last_end = last_end.max(mapping.end()).align_up(align);
        }
        for (&addr, mapping) in self.areas.range(last_end..) {
            if last_end.checked_add(size).is_some_and(|end| end <= addr) {
                return Some(last_end);
            }
            last_end = mapping.end().align_up(align);
        }
        last_end
            .checked_add(size)
            .is_some_and(|end| end <= limit.end)
            .then_some(last_end)
    }

    fn insert(&mut self, start: VirtAddr, size: usize, flags: MappingFlags) -> KResult {
        let range = VirtAddrRange::try_from_start_size(start, size).ok_or(KError::InvalidInput)?;
        if self.overlaps(range) {
            return Err(KError::AlreadyExists);
        }
        let old = self.areas.insert(start, KernelMapping { range, flags });
        debug_assert!(old.is_none());
        Ok(())
    }

    fn unmap(&mut self, start: VirtAddr, size: usize) {
        if size == 0 {
            return;
        }
        let range = VirtAddrRange::from_start_size(start, size);
        let mut retained = BTreeMap::new();
        for mapping in self.areas.values().copied() {
            if !mapping.range.overlaps(range) {
                retained.insert(mapping.start(), mapping);
                continue;
            }
            if mapping.start() < range.start {
                let left_range =
                    VirtAddrRange::new(mapping.start(), range.start.min(mapping.end()));
                retained.insert(
                    left_range.start,
                    KernelMapping {
                        range: left_range,
                        flags: mapping.flags,
                    },
                );
            }
            if mapping.end() > range.end {
                let right_range = VirtAddrRange::new(range.end.max(mapping.start()), mapping.end());
                retained.insert(
                    right_range.start,
                    KernelMapping {
                        range: right_range,
                        flags: mapping.flags,
                    },
                );
            }
        }
        self.areas = retained;
    }

    fn protect(&mut self, start: VirtAddr, size: usize, flags: MappingFlags) {
        if size == 0 {
            return;
        }
        let range = VirtAddrRange::from_start_size(start, size);
        let mut updated = BTreeMap::new();
        for mapping in self.areas.values().copied() {
            if !mapping.range.overlaps(range) {
                updated.insert(mapping.start(), mapping);
                continue;
            }
            if mapping.start() < range.start {
                let left = VirtAddrRange::new(mapping.start(), range.start.min(mapping.end()));
                updated.insert(
                    left.start,
                    KernelMapping {
                        range: left,
                        flags: mapping.flags,
                    },
                );
            }
            let middle_start = mapping.start().max(range.start);
            let middle_end = mapping.end().min(range.end);
            if middle_start < middle_end {
                let middle = VirtAddrRange::new(middle_start, middle_end);
                updated.insert(
                    middle.start,
                    KernelMapping {
                        range: middle,
                        flags,
                    },
                );
            }
            if mapping.end() > range.end {
                let right = VirtAddrRange::new(range.end.max(mapping.start()), mapping.end());
                updated.insert(
                    right.start,
                    KernelMapping {
                        range: right,
                        flags: mapping.flags,
                    },
                );
            }
        }
        self.areas = updated;
    }
}

/// The global kernel virtual address space.
///
/// Unlike [`UserMmSpace`], this type has no userspace VMA tree or process
/// lifetime state.  Its interval set records mappings managed through this
/// API (static linear mappings and `iomap`).  `vmalloc` owns a separate
/// interval table for its disjoint VA window rather than pretending that its
/// allocations are VMAs.
pub struct KernelAddressSpace {
    range: VirtAddrRange,
    pgtbl: PageTable,
    mappings: KernelMappingSet,
}

impl KernelAddressSpace {
    /// Creates an empty kernel page table covering `[base, base + size)`.
    pub fn new_empty(base: VirtAddr, size: usize) -> KResult<Self> {
        let pgtbl = PageTable::try_new_kernel().map_err(|_| KError::NoMemory)?;
        let range = VirtAddrRange::try_from_start_size(base, size).ok_or(KError::InvalidInput)?;
        Ok(Self {
            range,
            pgtbl,
            mappings: KernelMappingSet::default(),
        })
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
    /// shootdown outside `KERNEL_ASPACE`.  The generic `KernelMappingSet` is
    /// not updated automatically.  `vmalloc` is the only production user;
    /// kernel TLB integration tests also use it for synthetic mappings.  Such
    /// callers must keep the `USER` permission clear.
    pub const fn page_table_mut(&mut self) -> &mut PageTable {
        &mut self.pgtbl
    }

    /// Returns the page-table root physical address.
    pub const fn page_table_root(&self) -> PhysAddr {
        self.pgtbl.root_paddr()
    }

    /// Finds a free kernel VA interval.
    pub fn find_free_area(
        &self,
        hint: VirtAddr,
        size: usize,
        limit: VirtAddrRange,
        align: usize,
    ) -> Option<VirtAddr> {
        if size == 0
            || !is_aligned_4k(size)
            || align < PAGE_SIZE_4K
            || !align.is_power_of_two()
            || !self.range.contains_range(limit)
        {
            return None;
        }
        self.mappings.find_free_area(hint, size, limit, align)
    }

    /// Finds an explicitly recorded kernel mapping containing `vaddr`.
    pub fn find_mapping(&self, vaddr: VirtAddr) -> Option<&KernelMapping> {
        self.mappings.find(vaddr)
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
        let range = VirtAddrRange::from_start_size(start_vaddr, size);
        if self.mappings.overlaps(range) {
            return Err(KError::AlreadyExists);
        }
        // Reserve the VA interval before touching the page table.  This makes
        // the interval set the source of truth while the transaction is in
        // flight and lets the rollback path restore both representations.
        self.mappings.insert(start_vaddr, size, flags)?;

        let mut modify = self.pgtbl.modify();
        for offset in (0..size).step_by(PAGE_SIZE_4K) {
            let va = start_vaddr + offset;
            let pa = start_paddr + offset;
            if let Err(err) = modify.map(va, pa, PageSize::Size4K, flags) {
                for rollback in (0..offset).step_by(PAGE_SIZE_4K) {
                    let _ = modify.unmap(start_vaddr + rollback);
                }
                modify.finish_no_flush();
                self.mappings.unmap(start_vaddr, size);
                return Err(map_paging_err(err));
            }
        }
        modify.finish_no_flush();
        Ok(())
    }

    /// Removes the PTEs for a kernel mapping but keeps its VA interval
    /// reserved until the caller has completed the required TLB shootdown.
    pub fn unmap_no_release(&mut self, start: VirtAddr, size: usize) -> KResult {
        self.validate_region(start, size)?;
        let range = VirtAddrRange::from_start_size(start, size);
        if !self.mappings.covers(range) {
            return Err(KError::BadAddress);
        }
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

    /// Releases the metadata for a range whose PTE teardown and TLB flush are
    /// already complete.
    pub fn release_mapping(&mut self, start: VirtAddr, size: usize) -> KResult {
        self.validate_region(start, size)?;
        let range = VirtAddrRange::from_start_size(start, size);
        if !self.mappings.covers(range) {
            return Err(KError::BadAddress);
        }
        self.mappings.unmap(start, size);
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
        let range = VirtAddrRange::from_start_size(start, size);
        if !self.mappings.covers(range) {
            return Err(KError::BadAddress);
        }
        let mut modify = self.pgtbl.modify();
        if let Err(err) = modify.protect_region(start, size, flags) {
            modify.finish_no_flush();
            return Err(map_paging_err(err));
        }
        modify.finish_no_flush();
        self.mappings.protect(start, size, flags);
        Ok(())
    }
}

impl fmt::Debug for KernelAddressSpace {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("KernelAddressSpace")
            .field("va_range", &self.range)
            .field("page_table_root", &self.pgtbl.root_paddr())
            .field("mappings", &self.mappings.areas.len())
            .finish()
    }
}

#[cfg(unittest)]
mod tests {
    use khal::paging::MappingFlags;
    use memaddr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
    use unittest::def_test;

    use super::KernelMappingSet;

    #[def_test]
    fn kernel_mapping_set_tracks_ranges_without_vmas() {
        let mut mappings = KernelMappingSet::default();
        let base = VirtAddr::from_usize(0x10_000);
        let flags = MappingFlags::READ | MappingFlags::WRITE;
        mappings
            .insert(base, PAGE_SIZE_4K * 3, flags)
            .expect("first kernel mapping should fit");
        assert!(mappings.find(base + PAGE_SIZE_4K).is_some());
        assert!(mappings.covers(VirtAddrRange::from_start_size(base, PAGE_SIZE_4K * 3)));
        assert_eq!(
            mappings.find_free_area(
                base,
                PAGE_SIZE_4K,
                VirtAddrRange::from_start_size(base, PAGE_SIZE_4K * 8),
                PAGE_SIZE_4K,
            ),
            Some(base + PAGE_SIZE_4K * 3)
        );

        mappings.protect(base + PAGE_SIZE_4K, PAGE_SIZE_4K, MappingFlags::READ);
        assert_eq!(
            mappings
                .find(base + PAGE_SIZE_4K)
                .expect("protected interval should remain recorded")
                .flags(),
            MappingFlags::READ
        );
        mappings.unmap(base + PAGE_SIZE_4K, PAGE_SIZE_4K);
        assert!(mappings.find(base + PAGE_SIZE_4K).is_none());
        assert!(mappings.find(base).is_some());
        assert!(mappings.find(base + PAGE_SIZE_4K * 2).is_some());
        assert!(!mappings.covers(VirtAddrRange::from_start_size(base, PAGE_SIZE_4K * 3)));
    }
}
