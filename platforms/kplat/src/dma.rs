// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA lifecycle hooks.

use kplat_macros::device_interface;

#[device_interface]
pub trait PlatformDmaIf {
    /// Whether ordinary cached linear-map memory is directly device-accessible.
    ///
    /// Returning true guarantees coherent DMA in both directions for every
    /// device using this platform DMA backend, without cache maintenance,
    /// page-attribute changes, or page sharing. Platforms requiring any of
    /// those operations must keep the default false and use coherent buffers.
    fn can_dma_direct() -> bool {
        false
    }

    /// Finalize a DMA mapping after page attributes have been updated.
    fn prepare(_pa: usize, _size: usize) -> kerrno::KResult {
        Ok(())
    }

    /// Tear down any platform-specific DMA state before the pages are released.
    fn release(_pa: usize, _size: usize) -> kerrno::KResult {
        Ok(())
    }
}
