// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kalloc::{FrameState, UsageKind, global_allocator};
use khal::paging::MappingFlags;
use memaddr::{PAGE_SIZE_4K as PAGE, VirtAddr};
use unittest::def_test;

use super::{FutexBacking, MmSpace};
use crate::VmBackingKind;

#[def_test]
fn owned_linear_child_retains_storage_after_parent_unmaps() {
    let start = VirtAddr::from_usize(0x200000);
    let frames = global_allocator().alloc_frame_run(3, PAGE, UsageKind::VirtMem).unwrap();
    let observer = frames.try_clone_frame(0).unwrap();
    let meta = global_allocator().frame_meta(observer.pfn()).unwrap();
    let mut parent = MmSpace::new_empty_user(start, 64 * PAGE).unwrap();
    parent.map_owned_linear(start, frames, 3 * PAGE, MappingFlags::READ | MappingFlags::WRITE | MappingFlags::USER).unwrap();
    assert!(matches!(parent.find_vma(start).unwrap().backing().kind(), VmBackingKind::Linear));
    assert!(matches!(parent.resolve_futex_backing(start.as_usize()).unwrap(), FutexBacking::Private { .. }));
    parent.write(start, b"parent").unwrap();
    let child = parent.try_clone().unwrap();
    let mut data = [0u8; 6];
    child.lock().read(start, &mut data).unwrap();
    assert_eq!(&data, b"parent");
    child.lock().write(start, b"shared").unwrap();
    parent.read(start, &mut data).unwrap();
    assert_eq!(&data, b"shared");
    assert_eq!(meta.state(), FrameState::Referenced { references: 4 });
    parent.unmap(start, 3 * PAGE).unwrap();
    drop(parent);
    assert_eq!(meta.state(), FrameState::Referenced { references: 3 });
    child.lock().read(start, &mut data).unwrap();
    assert_eq!(&data, b"shared");
    drop(child);
    assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
}


#[def_test]
fn adjacent_owned_linear_sources_do_not_merge_or_lose_refault_backing() {
    let start = VirtAddr::from_usize(0x500000);
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    let mut space = MmSpace::new_empty_user(start, 64 * PAGE).unwrap();
    let original = global_allocator().alloc_frame_run(4, PAGE, UsageKind::VirtMem).unwrap();
    space.map_owned_linear(start, original, 4 * PAGE, flags).unwrap();
    space.write(start + 2 * PAGE, b"original").unwrap();
    space.unmap(start, 2 * PAGE).unwrap();
    let replacement = global_allocator().alloc_frame_run(2, PAGE, UsageKind::VirtMem).unwrap();
    space.map_owned_linear(start, replacement, 2 * PAGE, flags).unwrap();
    space.write(start, b"replaced").unwrap();
    space.vmas.merge_adjacent(start).unwrap();
    assert_eq!(space.vmas().count(), 2,
        "different backing owners must not merge after trimming the adjacent source");
    space.invalidate_present(start, 4 * PAGE).unwrap();
    space.populate_area(start, 4 * PAGE, flags).unwrap();
    let mut data = [0u8; 8];
    space.read(start, &mut data).unwrap();
    assert_eq!(&data, b"replaced");
    space.read(start + 2 * PAGE, &mut data).unwrap();
    assert_eq!(&data, b"original");
}
