// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA lifecycle hooks.

pub use kiface_mm_api::DmaCacheIf;
use kplat_macros::device_interface;
pub use memaddr::VirtAddr;

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
