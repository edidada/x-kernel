// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc_engine::AllocError;
use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddr, VirtAddr};
use unittest::def_test;

use super::{FramePlan, FrameRegion, MAX_REGIONS};

fn region(start: usize, pages: usize) -> FrameRegion {
    FrameRegion::new(
        PhysAddr::from_usize(start),
        VirtAddr::from_usize(start),
        pages * PAGE,
    )
}

#[def_test]
fn plan_preserves_holes_and_excludes_backing() {
    let plan = FramePlan::new(&[region(512 * PAGE, 64), region(256 * PAGE, 64)]).unwrap();
    assert_eq!(plan.count, 2);
    assert_eq!(plan.layout.described_pages(), 128);
    assert_eq!(plan.regions[0].paddr.as_usize(), 256 * PAGE);
    assert_eq!(plan.regions[1].paddr.as_usize(), 512 * PAGE);
    let (_, backing, inventory) = plan.into_inventory();
    let remaining: usize = inventory.map(|r| r.size_bytes / PAGE).sum();
    assert_eq!(remaining + backing.size_bytes / PAGE, 128);
}

#[def_test]
fn plan_rounds_inward_and_merges_adjacent_ram() {
    let first = FrameRegion::new(
        PhysAddr::from_usize(PAGE + 1),
        VirtAddr::from_usize(PAGE + 1),
        4 * PAGE - 1,
    );
    let plan = FramePlan::new(&[first, region(5 * PAGE, 4)]).unwrap();
    assert_eq!(plan.count, 1);
    assert_eq!(plan.regions[0].paddr.as_usize(), 2 * PAGE);
    assert_eq!(plan.layout.described_pages(), 7);
}

#[def_test]
fn physical_adjacency_does_not_merge_disjoint_access_mappings() {
    let second = FrameRegion::new(
        PhysAddr::from_usize(9 * PAGE),
        VirtAddr::from_usize(64 * PAGE),
        8 * PAGE,
    );
    let plan = FramePlan::new(&[region(PAGE, 8), second]).unwrap();
    assert_eq!(plan.count, 2);
    assert_eq!(plan.regions[0].size_bytes, 8 * PAGE);
    assert_eq!(plan.regions[1].vaddr, second.vaddr);
    assert_eq!(plan.layout.described_pages(), 16);
    // The current backend still requires one linear mapping. Its handoff
    // rejects this layout independently of physical coverage planning.
    assert!(matches!(
        crate::frame_mapping::LinearFrameMapping::new(&plan.regions[..plan.count]),
        Err(AllocError::InvalidInput)
    ));
}

#[def_test]
fn plan_rejects_overlap_and_overflow() {
    assert!(matches!(
        FramePlan::new(&[region(PAGE, 8), region(2 * PAGE, 8)]),
        Err(AllocError::MemoryOverlap)
    ));
    let overflow = FrameRegion::new(
        PhysAddr::from_usize(usize::MAX - PAGE),
        VirtAddr::from_usize(usize::MAX - PAGE),
        2 * PAGE,
    );
    assert!(matches!(
        FramePlan::new(&[overflow]),
        Err(AllocError::InvalidInput)
    ));
}

#[def_test]
fn plan_rejects_empty_tiny_and_excessive_regions() {
    assert!(matches!(FramePlan::new(&[]), Err(AllocError::NoMemory)));
    assert!(matches!(
        FramePlan::new(&[region(PAGE, 1)]),
        Err(AllocError::NoMemory)
    ));
    let regions =
        core::array::from_fn::<_, { MAX_REGIONS + 1 }, _>(|i| region((i * 16 + 1) * PAGE, 8));
    assert!(matches!(
        FramePlan::new(&regions),
        Err(AllocError::NoMemory)
    ));
}
