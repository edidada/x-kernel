// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA preparation hooks.

#[cfg(feature = "kvm-guest-mem-share")]
use khal::mem::PAGE_SIZE_4K;
use kplat::dma::PlatformDmaIf;

#[kiface::provide]
impl PlatformDmaIf {
    fn can_dma_direct() -> bool {
        // Enforce the capability boundary even if a seed config explicitly
        // enables direct DMA for an unsupported machine or protected guest.
        kbuild_config::DMA_DIRECT_COHERENT
            && kbuild_config::MACHINE_AARCH64_QEMU
            && !cfg!(feature = "kvm-guest-mem-share")
            && !kbuild_config::KFEAT_VIRTCCA_HUK_KEY
    }

    fn prepare(paddr: usize, size: usize) -> kerrno::KResult {
        #[cfg(feature = "kvm-guest-mem-share")]
        dma_share_pages(paddr, size);
        #[cfg(not(feature = "kvm-guest-mem-share"))]
        let _ = (paddr, size);
        Ok(())
    }

    fn release(paddr: usize, size: usize) -> kerrno::KResult {
        #[cfg(feature = "kvm-guest-mem-share")]
        dma_unshare_pages(paddr, size);
        #[cfg(not(feature = "kvm-guest-mem-share"))]
        let _ = (paddr, size);
        Ok(())
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
