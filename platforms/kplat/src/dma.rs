// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA lifecycle hooks.

use kplat_macros::device_interface;

/// Transfer direction for a platform DMA query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaDirection {
    /// The device reads CPU-produced data.
    DriverToDevice,
    /// The device writes data for the CPU.
    DeviceToDriver,
    /// The device both reads and writes the buffer.
    Bidirectional,
}

#[device_interface]
pub trait PlatformDmaIf {
    /// Bus address of `paddr..paddr+size` when the device can use it in place.
    ///
    /// `None` means bounce. The default is `None`.
    fn direct_dma_addr(_paddr: usize, _size: usize, _direction: DmaDirection) -> Option<usize> {
        None
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
