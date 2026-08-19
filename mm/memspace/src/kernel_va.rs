// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dynamic kernel virtual-address allocation.
//!
//! The kernel has several dynamic VA windows, notably `vmalloc` and `iomap`.
//! They share one interval allocator while retaining separate mapping
//! descriptors and lifetime policies.  Fixed/formula mappings such as the
//! direct map are intentionally not allocated here.

use alloc::collections::BTreeMap;

use kaddr_layout::{IOMAP_VADDR, IOMAP_VSIZE, VMALLOC_BASE, VMALLOC_END};
use kerrno::{KError, KResult};
use kspin::SpinNoIrq;
use memaddr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr, VirtAddrRange};

/// An occupied interval in the dynamic kernel VA allocator.
///
/// The fields are private so callers cannot manufacture a reservation for an
/// interval they did not obtain from [`KernelVaAllocator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KernelVaReservation {
    start: VirtAddr,
    size: usize,
}

impl KernelVaReservation {
    pub(crate) const fn start(self) -> VirtAddr {
        self.start
    }

    pub(crate) fn end(self) -> VirtAddr {
        self.start + self.size
    }

    pub(crate) fn range(self) -> VirtAddrRange {
        VirtAddrRange::from_start_size(self.start, self.size)
    }
}

/// Shared interval allocator for dynamic kernel VA windows.
///
/// The allocator tracks VA ownership only.  Mapping flags, physical backing,
/// and teardown state belong to the vmalloc/iomap descriptors that own each
/// reservation.
#[derive(Default)]
pub(crate) struct KernelVaAllocator {
    areas: BTreeMap<VirtAddr, KernelVaReservation>,
}

impl KernelVaAllocator {
    /// Reserves the lowest aligned free interval inside `limit`.
    pub(crate) fn reserve_in(
        &mut self,
        limit: VirtAddrRange,
        size: usize,
        align: usize,
    ) -> KResult<KernelVaReservation> {
        self.validate_request(limit, size, align)?;

        let mut candidate = limit.start.align_up(align);
        for (&start, area) in self.areas.range(..limit.end) {
            let area_end = area.end();
            if area_end <= candidate {
                continue;
            }

            if candidate
                .checked_add(size)
                .is_some_and(|end| end <= start && end <= limit.end)
            {
                return self.insert(candidate, size);
            }

            candidate = area_end.align_up(align);
            if candidate >= limit.end {
                return Err(KError::NoMemory);
            }
        }

        if candidate
            .checked_add(size)
            .is_some_and(|end| end <= limit.end)
        {
            self.insert(candidate, size)
        } else {
            Err(KError::NoMemory)
        }
    }

    /// Reserves an explicitly assigned interval inside a dynamic window.
    pub(crate) fn reserve_fixed_in(
        &mut self,
        limit: VirtAddrRange,
        start: VirtAddr,
        size: usize,
    ) -> KResult<KernelVaReservation> {
        self.validate_request(limit, size, PAGE_SIZE_4K)?;
        if !start.is_aligned_4k() {
            return Err(KError::InvalidInput);
        }
        let range = VirtAddrRange::try_from_start_size(start, size).ok_or(KError::InvalidInput)?;
        if !limit.contains_range(range) {
            return Err(KError::InvalidInput);
        }
        if self.overlaps(range) {
            return Err(KError::AlreadyExists);
        }
        self.insert(start, size)
    }

    /// Releases a previously reserved interval after its TLB teardown.
    pub(crate) fn release(&mut self, reservation: KernelVaReservation) -> KResult {
        let Some(current) = self.areas.get(&reservation.start) else {
            return Err(KError::BadAddress);
        };
        if *current != reservation {
            return Err(KError::BadAddress);
        }
        self.areas.remove(&reservation.start);
        Ok(())
    }

    /// Releases an interval identified by its exact range.
    pub(crate) fn release_range(&mut self, start: VirtAddr, size: usize) -> KResult {
        self.release(KernelVaReservation { start, size })
    }

    fn validate_request(&self, limit: VirtAddrRange, size: usize, align: usize) -> KResult {
        if limit.start >= limit.end
            || size == 0
            || !size.is_multiple_of(PAGE_SIZE_4K)
            || align < PAGE_SIZE_4K
            || !align.is_power_of_two()
            || !limit.start.is_aligned_4k()
            || !limit.end.is_aligned_4k()
        {
            return Err(KError::InvalidInput);
        }
        Ok(())
    }

    fn overlaps(&self, range: VirtAddrRange) -> bool {
        self.areas
            .range(..range.end)
            .next_back()
            .is_some_and(|(_, area)| area.end() > range.start)
            || self
                .areas
                .range(range.start..)
                .next()
                .is_some_and(|(_, area)| area.start < range.end)
    }

    fn insert(&mut self, start: VirtAddr, size: usize) -> KResult<KernelVaReservation> {
        let reservation = KernelVaReservation { start, size };
        if self.overlaps(reservation.range()) {
            return Err(KError::AlreadyExists);
        }
        let old = self.areas.insert(start, reservation);
        debug_assert!(old.is_none());
        Ok(reservation)
    }
}

const fn empty_allocator() -> KernelVaAllocator {
    KernelVaAllocator {
        areas: BTreeMap::new(),
    }
}

pub(crate) static KERNEL_VA_ALLOCATOR: SpinNoIrq<KernelVaAllocator> =
    SpinNoIrq::new(empty_allocator());

pub(crate) fn iomap_window() -> VirtAddrRange {
    VirtAddrRange::from_start_size(VirtAddr::from_usize(IOMAP_VADDR), IOMAP_VSIZE)
}

pub(crate) fn vmalloc_window() -> VirtAddrRange {
    VirtAddrRange::from_start_size(
        VirtAddr::from_usize(VMALLOC_BASE),
        VMALLOC_END - VMALLOC_BASE,
    )
}

#[cfg(unittest)]
mod tests {
    use memaddr::{VirtAddrRange, va};
    use unittest::def_test;

    use super::KernelVaAllocator;

    #[def_test]
    fn allocator_reserves_and_releases_disjoint_intervals() {
        let mut allocator = KernelVaAllocator::default();
        let window = VirtAddrRange::from_start_size(va!(0x10_000), 0x10_000);
        let first = allocator
            .reserve_in(window, 0x2000, 0x1000)
            .expect("first reservation");
        let second = allocator
            .reserve_in(window, 0x1000, 0x1000)
            .expect("second reservation");
        assert_eq!(first.start(), va!(0x10_000));
        assert_eq!(second.start(), va!(0x12_000));

        allocator.release(first).expect("release first");
        let reused = allocator
            .reserve_in(window, 0x1000, 0x1000)
            .expect("reuse released interval");
        assert_eq!(reused.start(), va!(0x10_000));
    }

    #[def_test]
    fn fixed_reservation_rejects_overlap_and_out_of_window() {
        let mut allocator = KernelVaAllocator::default();
        let window = VirtAddrRange::from_start_size(va!(0x20_000), 0x4000);
        allocator
            .reserve_fixed_in(window, va!(0x21_000), 0x1000)
            .expect("fixed reservation");
        assert!(
            allocator
                .reserve_fixed_in(window, va!(0x21_000), 0x1000)
                .is_err()
        );
        assert!(
            allocator
                .reserve_fixed_in(window, va!(0x24_000), 0x1000)
                .is_err()
        );
    }
}
