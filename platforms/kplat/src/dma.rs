// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA lifecycle hooks.

use kplat_macros::device_interface;

/// Direction of a temporary DMA mapping.
///
/// This is the platform query's view of the transfer. `kdma` keeps its own
/// public direction type and converts at the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaDirection {
    /// The device reads data the CPU produced.
    DriverToDevice,
    /// The device produces data the CPU reads.
    DeviceToDriver,
    /// The device both reads and writes the buffer.
    Bidirectional,
}

#[device_interface]
pub trait PlatformDmaIf {
    /// Bus address for `paddr..paddr+size` when that range can be handed to a
    /// device without bounce, cache maintenance, page sharing, or attribute
    /// changes.
    ///
    /// `None` means the caller must bounce. The default is `None`; a platform
    /// returns `Some` only for a verified coherent guest. Machine identity
    /// stays inside that platform. Device DMA masks and IOMMU translation are
    /// not part of this query.
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
