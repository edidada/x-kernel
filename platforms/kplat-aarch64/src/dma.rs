// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA preparation hooks.
//!
//! A range succeeds only when every hypercall succeeds. Attempt the complete
//! range even after an error so release can clean remaining pages; report an
//! error so the DMA owner retains backing whose state could not be restored.

#[cfg(any(feature = "kvm-guest-mem-share", unittest))]
use khal::mem::PAGE_SIZE_4K;
#[cfg(feature = "kvm-guest-mem-share")]
use kplat::dma::PlatformDmaIf;

#[cfg(feature = "kvm-guest-mem-share")]
#[kiface::provide]
impl PlatformDmaIf {
    fn prepare(paddr: usize, size: usize) -> kerrno::KResult {
        transition_dma_pages(
            paddr,
            size,
            crate::peripherals::kvm::guest_mem_share_page,
            "share",
        )
    }

    fn release(paddr: usize, size: usize) -> kerrno::KResult {
        transition_dma_pages(
            paddr,
            size,
            crate::peripherals::kvm::guest_mem_unshare_page,
            "unshare",
        )
    }
}

#[cfg(not(feature = "kvm-guest-mem-share"))]
kplat::default_dma_if_impl!();

#[cfg(any(feature = "kvm-guest-mem-share", unittest))]
fn transition_dma_pages(
    paddr: usize,
    size: usize,
    mut transition: impl FnMut(usize) -> bool,
    operation: &str,
) -> kerrno::KResult {
    if !paddr.is_multiple_of(PAGE_SIZE_4K)
        || !size.is_multiple_of(PAGE_SIZE_4K)
        || paddr.checked_add(size).is_none()
    {
        return Err(kerrno::KError::InvalidInput);
    }
    let mut result = Ok(());
    for offset in (0..size).step_by(PAGE_SIZE_4K) {
        let page_paddr = paddr + offset;
        if !transition(page_paddr) {
            log::warn!("cannot DMA {operation} page 0x{page_paddr:x}");
            result = Err(kerrno::KError::Io);
        }
    }
    result
}

unittest::path_mod_test!(dma_unittest, "../ut-tests/dma_unittest.rs");
