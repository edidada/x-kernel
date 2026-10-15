// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hardware-free tests for the VMM's host backing ownership.

use kalloc::{FrameState, GlobalPage, global_allocator};
use khal::mem::PhysAddr;
use kspin::NoPreemptIrqSave;
const PAGE: usize = 4096;
use unittest::def_test;

#[def_test]
fn guest_ram_reservation_owns_rounded_backing_and_zeroes_visible_bytes() {
    let guard = NoPreemptIrqSave::new();
    let reservation = super::reserve_guest_ram(0x4000_0000, (3 * PAGE) as u64).unwrap();
    let pfn = reservation.hpa_base() as usize / PAGE;
    for index in 0..4 {
        let meta = global_allocator().frame_meta(pfn + index).unwrap();
        assert_eq!(meta.state(), FrameState::Referenced { references: 1 });
    }
    let mapping = reservation.frames.kernel_addr();
    // SAFETY: this unpublished reservation owns the entire mapped backing and
    // no vCPU or hardware mapper has been created. The read ends before Drop.
    let data = unsafe { core::slice::from_raw_parts(mapping.as_ptr(), 3 * PAGE) };
    assert!(data.iter().all(|byte| *byte == 0));
    let retained = reservation.frames.try_clone_frame(3).unwrap();
    drop(reservation);
    assert_eq!(retained.phys_addr(), PhysAddr::from_usize((pfn + 3) * PAGE));
    assert_eq!(
        global_allocator().frame_meta(pfn + 3).unwrap().state(),
        FrameState::Referenced { references: 1 }
    );
    drop(retained);
    drop(guard);
    assert!(super::reserve_guest_ram(0, 0).is_none());
    assert!(super::reserve_guest_ram(0, (PAGE + 1) as u64).is_none());
}

#[def_test]
fn table_raw_owner_is_restored_and_retired_without_va_identity() {
    let _guard = NoPreemptIrqSave::new();
    let page = GlobalPage::alloc_zero().unwrap();
    let pa = page.start_pa();
    let (raw, count) = page.into_raw();
    assert_eq!(count, 1);
    assert_eq!(pa, raw);
    assert_eq!(
        global_allocator()
            .frame_meta(pa.as_usize() / PAGE)
            .unwrap()
            .state(),
        FrameState::Referenced { references: 1 }
    );
    // SAFETY: raw transfers this exact one-page allocation once. The fixture
    // never gives its table to hardware, so all walkers are already excluded.
    unsafe { super::free_pt_page(raw.as_usize() as u64) };
    assert_eq!(
        global_allocator()
            .frame_meta(pa.as_usize() / PAGE)
            .unwrap()
            .state(),
        FrameState::Unreferenced
    );
}
