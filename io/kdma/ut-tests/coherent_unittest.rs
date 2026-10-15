// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Failure injection for coherent setup and retirement with real frame holds.

use alloc::vec::Vec;
use core::cell::RefCell;

use alloc_engine::{AllocError, AllocResult};
use kalloc::{FrameRun, FrameState, UsageKind, global_allocator};
use khal::paging::MappingFlags;
use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddr, VirtAddr};
use unittest::def_test;

use super::{DmaMemoryOps, PreparationStage, prepare_allocation, retire_allocation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Coherent,
    Prepare,
    Release,
    Cached,
}

struct Ops {
    paddr: PhysAddr,
    vaddr: VirtAddr,
    backing: usize,
    bytes: usize,
    events: RefCell<Vec<Event>>,
    failures: RefCell<Vec<Event>>,
}

impl Ops {
    fn new(run: &FrameRun<'_>, pages: usize, failures: &[Event]) -> Self {
        Self {
            paddr: run.phys_addr(),
            vaddr: run.kernel_addr(),
            backing: run.frame_count(),
            bytes: pages * PAGE,
            events: RefCell::new(Vec::new()),
            failures: RefCell::new(failures.to_vec()),
        }
    }

    fn step(&self, event: Event, bytes: usize) -> AllocResult {
        assert_eq!(bytes, self.bytes, "hooks cover visible pages, not rounding slack");
        // One observer and one working/transferred owner must survive every hook.
        self.assert_references(2);
        self.events.borrow_mut().push(event);
        let mut failures = self.failures.borrow_mut();
        if failures.first() == Some(&event) {
            failures.remove(0);
            return Err(AllocError::NoMemory);
        }
        Ok(())
    }

    fn assert_references(&self, references: usize) {
        for index in 0..self.backing {
            assert_eq!(
                global_allocator().frame_meta(self.paddr.as_usize() / PAGE + index).unwrap().state(),
                FrameState::Shared { references }
            );
        }
    }
}

impl DmaMemoryOps for Ops {
    fn protect(&self, address: VirtAddr, size: usize, flags: MappingFlags) -> AllocResult {
        assert_eq!(address, self.vaddr);
        let event = if flags.contains(MappingFlags::UNCACHED) {
            assert!(flags.contains(MappingFlags::SHARED));
            Event::Coherent
        } else {
            assert_eq!(flags, MappingFlags::READ | MappingFlags::WRITE);
            Event::Cached
        };
        self.step(event, size)
    }

    fn prepare(&self, address: PhysAddr, size: usize) -> AllocResult {
        assert_eq!(address, self.paddr);
        self.step(Event::Prepare, size)
    }

    fn release(&self, address: PhysAddr, size: usize) -> AllocResult {
        assert_eq!(address, self.paddr);
        self.step(Event::Release, size)
    }
}

fn new_run() -> FrameRun<'static> {
    global_allocator().alloc_dma_frame_run(3, PAGE, UsageKind::Dma).unwrap()
}

#[def_test]
fn coherent_transfer_keeps_all_backing_until_platform_and_tlb_retirement() {
    let frames = new_run();
    let observer = frames.try_clone_run().unwrap();
    let ops = Ops::new(&frames, 3, &[]);
    let info = prepare_allocation(frames, 3, &ops).unwrap();
    assert_eq!(info.bus_addr, crate::p2b(observer.phys_addr()));
    ops.assert_references(2);
    // SAFETY: prepare_allocation just transferred this complete run once;
    // this test never publishes its addresses to a device or hardware mapping.
    let restored = unsafe { global_allocator().frame_run_from_raw(ops.paddr, ops.backing) };
    retire_allocation(restored, 3, PreparationStage::Platform, &ops).unwrap();
    assert_eq!(*ops.events.borrow(), [Event::Coherent, Event::Prepare, Event::Release, Event::Cached]);
    ops.assert_references(1);
    drop(observer);
}

#[def_test]
fn failed_attribute_setup_restores_mapping_before_ending_backing() {
    let frames = new_run();
    let observer = frames.try_clone_run().unwrap();
    let ops = Ops::new(&frames, 3, &[Event::Coherent]);
    assert!(matches!(prepare_allocation(frames, 3, &ops), Err(AllocError::NoMemory)));
    assert_eq!(*ops.events.borrow(), [Event::Coherent, Event::Cached]);
    ops.assert_references(1);
    drop(observer);
}

#[def_test]
fn failed_prepare_releases_partial_platform_state_before_reuse() {
    let frames = new_run();
    let observer = frames.try_clone_run().unwrap();
    let ops = Ops::new(&frames, 3, &[Event::Prepare]);
    assert!(matches!(prepare_allocation(frames, 3, &ops), Err(AllocError::NoMemory)));
    assert_eq!(*ops.events.borrow(), [Event::Coherent, Event::Prepare, Event::Release, Event::Cached]);
    ops.assert_references(1);
    drop(observer);
}

#[def_test]
fn failed_platform_release_retains_the_complete_run() {
    let frames = new_run();
    let observer = frames.try_clone_run().unwrap();
    let ops = Ops::new(&frames, 3, &[Event::Prepare, Event::Release]);
    assert!(matches!(prepare_allocation(frames, 3, &ops), Err(AllocError::NoMemory)));
    assert_eq!(*ops.events.borrow(), [Event::Coherent, Event::Prepare, Event::Release]);
    ops.assert_references(2);
    // SAFETY: failed cleanup transferred exactly this known run into quarantine;
    // no hardware or real mapping hooks ran. Recover that one transfer for test
    // teardown after the one-shot release failure has cleared.
    let quarantined = unsafe { global_allocator().frame_run_from_raw(ops.paddr, ops.backing) };
    retire_allocation(quarantined, 3, PreparationStage::Platform, &ops).unwrap();
    ops.assert_references(1);
    drop(observer);
}

#[def_test]
fn failed_cached_restoration_keeps_backing_after_platform_release() {
    let frames = new_run();
    let observer = frames.try_clone_run().unwrap();
    let ops = Ops::new(&frames, 3, &[Event::Cached]);
    prepare_allocation(frames, 3, &ops).unwrap();
    // SAFETY: this consumes the successful setup's unique transfer; no device
    // has used it, and the observer is a separate, accounted lifetime hold.
    let restored = unsafe { global_allocator().frame_run_from_raw(ops.paddr, ops.backing) };
    assert_eq!(retire_allocation(restored, 3, PreparationStage::Platform, &ops), Err(AllocError::NoMemory));
    assert_eq!(*ops.events.borrow(), [Event::Coherent, Event::Prepare, Event::Release, Event::Cached]);
    ops.assert_references(2);
    // SAFETY: the failed restoration transferred this known run into quarantine.
    // Platform release already succeeded; only the simulated attributes remain.
    let quarantined = unsafe { global_allocator().frame_run_from_raw(ops.paddr, ops.backing) };
    retire_allocation(quarantined, 3, PreparationStage::Attributes, &ops).unwrap();
    assert_eq!(*ops.events.borrow(), [Event::Coherent, Event::Prepare, Event::Release, Event::Cached, Event::Cached]);
    ops.assert_references(1);
    drop(observer);
}
