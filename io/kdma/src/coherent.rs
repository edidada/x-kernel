// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Coherent mapping setup and retirement while retaining physical backing.

use core::ptr::NonNull;

use alloc_engine::{AllocError, AllocResult};
use kalloc::FrameRun;
use khal::paging::MappingFlags;
use memaddr::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

use crate::{DmaInfo, DmaPageTableIf, p2b};

/// Side effects that must finish before coherent RAM becomes reusable.
pub(super) trait DmaMemoryOps {
    fn protect(&self, address: VirtAddr, size: usize, flags: MappingFlags) -> AllocResult;
    fn prepare(&self, address: PhysAddr, size: usize) -> AllocResult;
    fn release(&self, address: PhysAddr, size: usize) -> AllocResult;
}

pub(super) struct PlatformDmaMemoryOps;

impl DmaMemoryOps for PlatformDmaMemoryOps {
    fn protect(&self, address: VirtAddr, size: usize, flags: MappingFlags) -> AllocResult {
        DmaPageTableIf::protect(address, size, flags).map_err(|_| {
            log::error!("DMA mapping attribute update failed");
            AllocError::NoMemory
        })
    }

    fn prepare(&self, address: PhysAddr, size: usize) -> AllocResult {
        kplat::dma::prepare(address.as_usize(), size).map_err(|_| {
            log::error!("platform DMA prepare failed");
            AllocError::NoMemory
        })
    }

    fn release(&self, address: PhysAddr, size: usize) -> AllocResult {
        kplat::dma::release(address.as_usize(), size).map_err(|_| {
            log::error!("platform DMA release failed");
            AllocError::NoMemory
        })
    }
}

/// How far preparation may have progressed, including partially failed hooks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PreparationStage {
    Attributes,
    Platform,
}

/// Publishes one raw run transfer only after coherent preparation succeeds.
///
/// The returned address view carries exactly one outstanding FrameRun transfer.
/// Its recipient must retain it through device use and restore it exactly once.
pub(super) fn prepare_allocation(
    frames: FrameRun<'static>,
    num_pages: usize,
    ops: &impl DmaMemoryOps,
) -> AllocResult<DmaInfo> {
    assert!(num_pages > 0 && num_pages <= frames.frame_count());
    let address = frames.kernel_addr();
    let size = num_pages * PAGE_SIZE_4K;
    let info = DmaInfo {
        cpu_addr: NonNull::new(address.as_mut_ptr()).ok_or(AllocError::InvalidInput)?,
        bus_addr: p2b(frames.phys_addr()),
    };
    let flags =
        MappingFlags::READ | MappingFlags::WRITE | MappingFlags::UNCACHED | MappingFlags::SHARED;
    if let Err(error) = ops.protect(address, size, flags) {
        // protect may have changed a prefix before reporting failure.
        let _ = retire_allocation(frames, num_pages, PreparationStage::Attributes, ops);
        return Err(error);
    }
    if let Err(error) = ops.prepare(frames.phys_addr(), size) {
        // A failed platform hook may also have shared a prefix of the range.
        let _ = retire_allocation(frames, num_pages, PreparationStage::Platform, ops);
        return Err(error);
    }
    let _ = frames.into_raw();
    Ok(info)
}

/// Ends a restored hold after platform release and synchronous flag restoration.
///
/// On failure an unreconstructed raw transfer permanently retains the complete
/// backing. Reusing still-shared or incorrectly cached RAM would be unsafe.
/// The caller must have stopped all device accesses before entering this path.
pub(super) fn retire_allocation(
    frames: FrameRun<'static>,
    num_pages: usize,
    stage: PreparationStage,
    ops: &impl DmaMemoryOps,
) -> AllocResult {
    assert!(num_pages > 0 && num_pages <= frames.frame_count());
    let size = num_pages * PAGE_SIZE_4K;
    let result = (|| {
        if stage == PreparationStage::Platform {
            ops.release(frames.phys_addr(), size)?;
        }
        ops.protect(
            frames.kernel_addr(),
            size,
            MappingFlags::READ | MappingFlags::WRITE,
        )
    })();
    if let Err(error) = result {
        let (address, count) = frames.into_raw();
        log::error!(
            "DMA cleanup failed; retaining backing at {:?} ({} base pages): {:?}",
            address,
            count,
            error
        );
        return Err(error);
    }
    drop(frames);
    Ok(())
}

unittest::path_mod_test!(coherent_unittest, "../ut-tests/coherent_unittest.rs");
