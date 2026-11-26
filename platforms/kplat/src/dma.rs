// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA lifecycle hooks.

use kplat_macros::device_interface;

#[device_interface]
pub trait DmaCacheIf {
    /// Pushes dirty cache lines covering `[vaddr, vaddr + len)` to the Point
    /// of Coherency without discarding them (clean, `dc cvac` on AArch64).
    ///
    /// Call it for `DMA_TO_DEVICE` mappings and `sync_for_device` before the
    /// device reads. `vaddr` names kernel linear-map memory owned by the DMA
    /// mapping; partial cache lines at either end pull neighbouring bytes
    /// into the maintenance window, so callers must not share those bytes
    /// with unrelated data unless the platform is coherent.
    fn clean_range(vaddr: usize, len: usize) {}

    /// Pushes and then discards cache lines covering `[vaddr, vaddr + len)`
    /// (clean + invalidate, `dc civac` on AArch64).
    ///
    /// Call it for `DMA_FROM_DEVICE` mappings and `sync_for_cpu` after the
    /// device wrote: clean-before-invalidate guarantees a dirty line cannot
    /// write stale CPU data back over the device's fresh results after the
    /// invalidation.
    fn clean_inval_range(vaddr: usize, len: usize) {}
}

#[device_interface]
pub trait PlatformDmaIf {
    /// Finalize a DMA mapping after page attributes have been updated.
    fn prepare(pa: usize, size: usize) -> kerrno::KResult {
        Ok(())
    }

    /// Tear down any platform-specific DMA state before the pages are released.
    fn release(pa: usize, size: usize) -> kerrno::KResult {
        Ok(())
    }
}
