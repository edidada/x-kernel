// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA preparation hooks.

#[cfg(feature = "kvm-guest-mem-share")]
use khal::mem::PAGE_SIZE_4K;
#[cfg(feature = "kvm-guest-mem-share")]
use kplat::dma::PlatformDmaIf;

#[cfg(feature = "kvm-guest-mem-share")]
#[kiface::provide]
impl PlatformDmaIf {
    fn prepare(paddr: usize, size: usize) -> kerrno::KResult {
        dma_share_pages(paddr, size);
        Ok(())
    }

    fn release(paddr: usize, size: usize) -> kerrno::KResult {
        dma_unshare_pages(paddr, size);
        Ok(())
    }
}

#[cfg(not(feature = "kvm-guest-mem-share"))]
kplat::default_dma_if_impl!();

/// Real cache maintenance for non-coherent DMA on AArch64.
///
/// Coherent platforms keep the mm-api default no-ops; here the SoC may run
/// non-coherent masters, so `dma_map_sg`/`dma_map_page` get actual `dc cvac`
/// and `dc civac` sequences over the kernel linear map.
#[kiface::provide]
impl kplat::dma::DmaCacheIf {
    fn clean_range(vaddr: usize, len: usize) {
        karch::dma_clean_range(vaddr.into(), len);
    }

    fn clean_inval_range(vaddr: usize, len: usize) {
        // Clean first, then drop the lines: a dirty line must not write back
        // over device-written data after the invalidation (the upstream
        // `dma_invalidate_range` caller contract).
        karch::dma_clean_range(vaddr.into(), len);
        karch::dma_invalidate_range(vaddr.into(), len);
    }
}

#[cfg(feature = "kvm-guest-mem-share")]
fn dma_unshare_pages(paddr: usize, size: usize) {
    let pages = size / PAGE_SIZE_4K;
    for page_index in 0..pages {
        let page_paddr = paddr + PAGE_SIZE_4K * page_index;
        if !crate::peripherals::kvm::guest_mem_unshare_page(page_paddr) {
            log::warn!("[virtio hal impl] cannot unshare 0x{page_paddr:x}");
        }
    }
}

#[cfg(feature = "kvm-guest-mem-share")]
fn dma_share_pages(paddr: usize, size: usize) {
    let pages = size / PAGE_SIZE_4K;
    for page_index in 0..pages {
        let page_paddr = paddr + PAGE_SIZE_4K * page_index;
        if !crate::peripherals::kvm::guest_mem_share_page(page_paddr) {
            log::warn!("[virtio hal impl] cannot share 0x{page_paddr:x}");
        }
    }
}
