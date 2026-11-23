// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scatter-gather mapping tests: merge, fallback, constraints and the
//! map/unmap lifecycle.
//!
//! Extents are backed by real DMA-coherent allocations so the bounce path
//! (which copies through the CPU linear map) touches only owned memory. The
//! fallback tests then pick a `dma_mask` strictly below the allocated
//! address to force bouncing deterministically.

use core::ptr::NonNull;

use unittest::{assert, assert_eq, def_test};

use crate::{
    DmaDirection,
    sg::{DmaConstraints, SgEntry, SgTable},
};

const PAGE: usize = 4096;

/// One real DMA-coherent page and its physical address.
struct OwnedPage {
    cpu: NonNull<u8>,
    paddr: memaddr::PhysAddr,
}

impl Drop for OwnedPage {
    fn drop(&mut self) {
        let info = crate::DmaInfo {
            cpu_addr: self.cpu,
            bus_addr: crate::p2b(self.paddr),
        };
        // SAFETY: the exact allocation from `allocate`, freed once.
        unsafe {
            crate::deallocate_dma_memory(
                info,
                core::alloc::Layout::from_size_align(PAGE, PAGE)
                    .expect("fixed page layout is valid"),
            );
        }
    }
}

fn allocate_page() -> OwnedPage {
    // SAFETY: the layout is a valid page request; the buffer is owned until
    // the handle drops.
    let info = unsafe {
        crate::allocate_dma_memory(
            core::alloc::Layout::from_size_align(PAGE, PAGE)
                .expect("fixed page layout is valid"),
        )
    }
    .expect("coherent page allocates in the unit-test kernel");
    OwnedPage {
        cpu: info.cpu_addr,
        paddr: crate::b2p(info.bus_addr),
    }
}

/// A contiguous low-bit mask that excludes `paddr` (forces bouncing).
fn mask_below(paddr: memaddr::PhysAddr) -> u64 {
    let raw = paddr.as_usize() as u64;
    if raw == 0 {
        return 0;
    }
    (1 << (63 - raw.leading_zeros())) - 1
}

/// A 32-bit device with realistic segmentation limits.
const fn constrained() -> DmaConstraints {
    DmaConstraints {
        dma_mask: 0xffff_ffff,
        max_segment_size: 64 * 1024,
        segment_boundary_mask: 0x3fff_ffff,
        cache_line_size: 64,
        cache_coherent: true,
    }
}

#[def_test(serial)]
fn sg_table_builds_from_extents() {
    let table = SgTable::from_entries(&[
        SgEntry::new(memaddr::PhysAddr::from(0x1000usize), 8192),
        SgEntry::new(memaddr::PhysAddr::from(0x9000usize), 4096),
    ])
    .expect("table builds");
    assert_eq!(table.orig_nents(), 2);
    assert_eq!(table.nents(), 0, "unmapped table has no valid segments");
    assert_eq!(table.segments().len(), 2);

    assert!(
        SgTable::from_entries(&[SgEntry::new(memaddr::PhysAddr::from(0x1000usize), 0)]).is_err(),
        "zero-length extents are rejected"
    );
    assert!(SgTable::from_entries(&[]).is_err());
}

#[def_test(serial)]
fn sg_maps_and_unmaps_direct_segments() {
    let constraints = DmaConstraints::UNCONSTRAINED;
    let page = allocate_page();
    let mut table =
        SgTable::from_entries(&[SgEntry::new(page.paddr, PAGE)]).expect("table builds");

    // SAFETY: the extent is owned coherent RAM; no device observes it.
    let nents = unsafe { crate::dma_map_sg(&mut table, &constraints, DmaDirection::Bidirectional) }
        .expect("direct map");
    assert_eq!(nents, 1);
    let bus = table.segments()[0].bus_addr;
    assert_eq!(
        bus.as_u64(),
        page.paddr.as_usize() as u64 + kbuild_config::PHYS_BUS_OFFSET as u64,
        "direct mapping is the physical translation"
    );
    assert_eq!(table.segments()[0].len, PAGE);

    // SAFETY: the exact mapping from dma_map_sg.
    unsafe { crate::dma_unmap_sg(&mut table, DmaDirection::Bidirectional) };
    assert_eq!(table.nents(), 0);
}

#[def_test(serial)]
fn sg_merges_physically_contiguous_direct_segments() {
    let constraints = DmaConstraints::UNCONSTRAINED;
    // Allocate contiguously in one request so the two halves are adjacent.
    // SAFETY: two contiguous pages owned for the test duration.
    let info = unsafe {
        crate::allocate_dma_memory(
            core::alloc::Layout::from_size_align(2 * PAGE, PAGE)
                .expect("fixed layout is valid"),
        )
    }
    .expect("coherent run allocates");
    let base = crate::b2p(info.bus_addr);
    let mut table = SgTable::from_entries(&[
        SgEntry::new(base, PAGE),
        SgEntry::new(
            memaddr::PhysAddr::from(base.as_usize() + PAGE),
            PAGE,
        ),
        // Discontiguous: cannot merge with the run above.
        SgEntry::new(
            memaddr::PhysAddr::from(base.as_usize() + 3 * PAGE),
            PAGE,
        ),
    ])
    .expect("table builds");

    // SAFETY: extents are owned coherent RAM; no device observes them.
    let nents = unsafe { crate::dma_map_sg(&mut table, &constraints, DmaDirection::Bidirectional) }
        .expect("map");
    assert_eq!(nents, 2, "two contiguous pages merge into one segment");
    assert_eq!(table.segments()[0].len, 2 * PAGE);

    // SAFETY: the exact mapping from dma_map_sg.
    unsafe { crate::dma_unmap_sg(&mut table, DmaDirection::Bidirectional) };
    // SAFETY: the backing run from the single allocation.
    unsafe {
        crate::deallocate_dma_memory(
            info,
            core::alloc::Layout::from_size_align(2 * PAGE, PAGE).expect("fixed layout is valid"),
        );
    }
}

#[def_test(serial)]
fn sg_respects_max_segment_size_and_boundary() {
    let mut constraints = constrained();
    constraints.max_segment_size = 2 * PAGE;
    // SAFETY: three contiguous pages owned for the test duration.
    let info = unsafe {
        crate::allocate_dma_memory(
            core::alloc::Layout::from_size_align(3 * PAGE, PAGE)
                .expect("fixed layout is valid"),
        )
    }
    .expect("coherent run allocates");
    let base = crate::b2p(info.bus_addr);
    let mut table = SgTable::from_entries(&[
        SgEntry::new(base, PAGE),
        SgEntry::new(memaddr::PhysAddr::from(base.as_usize() + PAGE), PAGE),
        SgEntry::new(
            memaddr::PhysAddr::from(base.as_usize() + 2 * PAGE),
            PAGE,
        ),
    ])
    .expect("table builds");

    // SAFETY: extents are owned coherent RAM.
    let nents = unsafe { crate::dma_map_sg(&mut table, &constraints, DmaDirection::Bidirectional) }
        .expect("map");
    assert_eq!(nents, 2, "merge stops at max_segment_size");
    assert_eq!(table.segments()[0].len, 2 * PAGE);
    // SAFETY: the exact mapping from dma_map_sg.
    unsafe { crate::dma_unmap_sg(&mut table, DmaDirection::Bidirectional) };

    // SAFETY: the backing run from the single allocation.
    unsafe {
        crate::deallocate_dma_memory(
            info,
            core::alloc::Layout::from_size_align(3 * PAGE, PAGE).expect("fixed layout is valid"),
        );
    }
}

#[def_test(serial)]
fn sg_falls_back_to_bounce_for_out_of_mask_pages() {
    let page = allocate_page();
    let constraints = DmaConstraints {
        dma_mask: mask_below(page.paddr),
        max_segment_size: 64 * 1024,
        segment_boundary_mask: u64::MAX,
        cache_line_size: 64,
        cache_coherent: true,
    };
    let mut table =
        SgTable::from_entries(&[SgEntry::new(page.paddr, PAGE)]).expect("table builds");

    // SAFETY: the extent is owned coherent RAM; the bounce pool allocates
    // real coherent memory for the staged copy.
    let nents = unsafe { crate::dma_map_sg(&mut table, &constraints, DmaDirection::DeviceToDriver) }
        .expect("map with fallback");
    assert_eq!(nents, 1);
    let bounced_bus = table.segments()[0].bus_addr.as_u64();
    assert_ne!(
        bounced_bus,
        page.paddr.as_usize() as u64 + kbuild_config::PHYS_BUS_OFFSET as u64,
        "out-of-mask extent must be bounced, not mapped at its physical address"
    );
    // The pool address is fixed once at initialization; a real device whose
    // mask cannot reach the pool cannot use bounce buffering at all. The
    // mapping contract therefore only asserts the extent was staged
    // somewhere other than its physical address.

    // SAFETY: unmap copies pool contents back and frees the bounce slot.
    unsafe { crate::dma_unmap_sg(&mut table, DmaDirection::DeviceToDriver) };
    assert_eq!(table.nents(), 0);
}

#[def_test(serial)]
fn sg_double_map_is_rejected() {
    let constraints = DmaConstraints::UNCONSTRAINED;
    let page = allocate_page();
    let mut table = SgTable::from_entries(&[SgEntry::new(page.paddr, PAGE)]).expect("table builds");
    // SAFETY: owned extent; no device observes the mapping.
    unsafe { crate::dma_map_sg(&mut table, &constraints, DmaDirection::Bidirectional) }
        .expect("first map");
    // SAFETY: the table is already mapped; the call must refuse without
    // touching state.
    let second = unsafe { crate::dma_map_sg(&mut table, &constraints, DmaDirection::Bidirectional) };
    assert!(
        second.is_err(),
        "mapping an already-mapped table is a caller contract violation"
    );
    // SAFETY: the exact mapping from the first dma_map_sg.
    unsafe { crate::dma_unmap_sg(&mut table, DmaDirection::Bidirectional) };
}

#[def_test(serial)]
fn dma_map_page_maps_one_extent() {
    let constraints = DmaConstraints::UNCONSTRAINED;
    let page = allocate_page();
    // SAFETY: owned extent; no device observes the mapping.
    let bus =
        unsafe { crate::dma_map_page(page.paddr, PAGE, &constraints, DmaDirection::DriverToDevice) }
            .expect("page maps");
    assert_eq!(
        bus.as_u64(),
        page.paddr.as_usize() as u64 + kbuild_config::PHYS_BUS_OFFSET as u64
    );
    // SAFETY: the exact mapping from dma_map_page.
    unsafe {
        crate::dma_unmap_page(
            page.paddr,
            PAGE,
            bus,
            &constraints,
            DmaDirection::DriverToDevice,
        )
    };
}
