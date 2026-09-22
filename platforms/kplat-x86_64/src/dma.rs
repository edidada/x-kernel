// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA capability for ordinary x86_64 QEMU guests.

use kplat::dma::{DmaDirection, PlatformDmaIf};

#[kiface::provide]
impl PlatformDmaIf {
    fn direct_dma_addr(paddr: usize, _size: usize, _direction: DmaDirection) -> Option<usize> {
        // Linux 6.8 `force_dma_unencrypted()` bounces SEV guests. This tree
        // has no TDX symbol; a non-zero SEV C-bit or the CSV HUK path keeps
        // bounce even if a seed config requests direct DMA.
        if !kbuild_config::DMA_DIRECT_COHERENT
            || kbuild_config::SEV_CBIT_POS != 0
            || kbuild_config::KFEAT_CSV_HUK_KEY
        {
            return None;
        }
        Some(paddr.wrapping_add(kbuild_config::PHYS_BUS_OFFSET))
    }

    fn prepare(_paddr: usize, _size: usize) -> kerrno::KResult {
        Ok(())
    }

    fn release(_paddr: usize, _size: usize) -> kerrno::KResult {
        Ok(())
    }
}
