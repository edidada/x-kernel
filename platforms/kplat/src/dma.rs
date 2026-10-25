// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA lifecycle hooks.

use kplat_macros::device_interface;

#[device_interface]
pub trait PlatformDmaIf {
    /// Finalize a DMA mapping after page attributes have been updated.
    /// On error, the caller invokes `release` for the entire attempted range;
    /// a provider may have prepared only a prefix. The device has not received
    /// the allocation yet. Providers must report failed preparation as an error.
    fn prepare(pa: usize, size: usize) -> kerrno::KResult {
        Ok(())
    }

    /// Tear down any platform-specific DMA state before the pages are released.
    /// Must also handle a partially prepared range after `prepare` fails.
    /// Return success only when the complete range is safe for reuse; on error
    /// the caller retains the pages. Providers should tolerate unprepared pages.
    fn release(pa: usize, size: usize) -> kerrno::KResult {
        Ok(())
    }
}
