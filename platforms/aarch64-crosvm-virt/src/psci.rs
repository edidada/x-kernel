// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PSCI wrappers and KVM guard-granule helpers.
use kplat::{dma::PlatformDmaIf, mmio::PlatformMmioIf};
use ktypes::Once;
pub static GUARD_GRANULE: Once<usize> = Once::new();
const ARM_SMCCC_VENDOR_HYP_KVM_MEM_UNSHARE_FUNC_ID: u32 =
    ((1) << 31) | ((1) << 30) | (((6) & 0x3F) << 24) | ((4) & 0xFFFF);
const ARM_SMCCC_VENDOR_HYP_KVM_MMIO_GUARD_INFO_FUNC_ID: u32 =
    ((1) << 31) | ((1) << 30) | (((6) & 0x3F) << 24) | ((5) & 0xFFFF);
const ARM_SMCCC_VENDOR_HYP_KVM_MEM_SHARE_FUNC_ID: u32 =
    ((1) << 31) | ((1) << 30) | (((6) & 0x3F) << 24) | ((3) & 0xFFFF);
/// Invoke a PSCI HVC call and return raw results.
pub fn psci_hvc_call(func: u32, arg0: usize, arg1: usize, arg2: usize) -> (usize, usize) {
    let ret0;
    let ret1;
    unsafe {
        core::arch::asm!(
            "hvc #0",
            inlateout("x0") func as usize => ret0,
            inlateout("x1") arg0 => ret1,
            in("x2") arg1,
            in("x3") arg2,
        )
    }
    (ret0, ret1)
}
/// Query and cache the KVM MMIO guard granule size.
///
/// When pvmfw has enrolled the VM into MMIO guard mode, this HVC returns the
/// guard granule size.  When no pvmfw ran (or the hypervisor does not support
/// MMIO guard), the call returns an error and we leave GUARD_GRANULE unset so
/// that later code can fall back to the unguarded path.
pub fn kvm_guard_granule_init() {
    let (guard_granule, guard_has_range) =
        psci_hvc_call(ARM_SMCCC_VENDOR_HYP_KVM_MMIO_GUARD_INFO_FUNC_ID, 0, 0, 0);
    // guard_granule is the page size; guard_has_range == 1 means range-based
    // guard is supported.  A negative / zero return means not enrolled.
    if guard_has_range == 0x1 && guard_granule > 0 {
        log::info!(
            "[mmio-guard] enrolled, granule = {:#x}",
            guard_granule
        );
        GUARD_GRANULE.call_once(|| guard_granule);
    } else {
        log::info!(
            "[mmio-guard] not active (granule={:#x}, has_range={:#x}), MMIO guard disabled",
            guard_granule, guard_has_range
        );
    }
}
fn __invoke_mmioguard(phys_addr: usize, _nr_granules: usize, map: bool) -> usize {
    let func_id: u32 = if map {
        ((1) << 31) | ((1) << 30) | (((6) & 0x3F) << 24) | ((10) & 0xFFFF)
    } else {
        ((1) << 31) | ((1) << 30) | (((6) & 0x3F) << 24) | ((11) & 0xFFFF)
    };
    let (result, done) = psci_hvc_call(func_id, phys_addr, 1, 0);
    if result != 0 {
        panic!();
    }
    done
}
fn __do_xmap_granules(phys_addr: usize, nr_granules: usize, map: bool) -> usize {
    let mut nr_xmapped = 0;
    let mut nr_granules = nr_granules as isize;
    let mut phys_addr = phys_addr;
    while nr_granules > 0 {
        let __nr_xmapped = __invoke_mmioguard(phys_addr, nr_granules as usize, map);
        nr_xmapped += __nr_xmapped;
        if __nr_xmapped as isize > nr_granules {
            break;
        }
        phys_addr += __nr_xmapped * { GUARD_GRANULE.get().unwrap() };
        nr_granules -= __nr_xmapped as isize;
    }
    nr_xmapped
}
/// Map a physical MMIO range via the KVM guard-granule interface.
pub fn do_xmap_granules(phys_addr: usize, size: usize) {
    let nr_granules = size / { GUARD_GRANULE.get().unwrap() };
    let ret = __do_xmap_granules(phys_addr, nr_granules, true);
    assert_eq!(ret, nr_granules);
}
struct DmaPlatformImpl;
struct MmioPlatformImpl;
#[impl_dev_interface]
impl PlatformDmaIf for DmaPlatformImpl {
    fn prepare(paddr: usize, size: usize) -> kerrno::KResult {
        dma_share_pages(paddr, size);
        Ok(())
    }

    fn release(paddr: usize, size: usize) -> kerrno::KResult {
        dma_unshare_pages(paddr, size);
        Ok(())
    }
}

#[impl_dev_interface]
impl PlatformMmioIf for MmioPlatformImpl {
    fn prepare(paddr: usize, size: usize) -> kerrno::KResult {
        if let Some(&granule) = GUARD_GRANULE.get() {
            // pvmfw enrolled MMIO guard — must map each granule via HVC before
            // the guest stage-1 mapping becomes usable.
            let aligned_start = paddr & !(granule - 1);
            let aligned_end = (paddr + size + granule - 1) & !(granule - 1);
            let nr_granules = (aligned_end - aligned_start) / granule;
            log::debug!(
                "[mmio-guard] mapping paddr={:#x} size={:#x} ({} granules)",
                aligned_start, aligned_end - aligned_start, nr_granules
            );
            do_xmap_granules(aligned_start, aligned_end - aligned_start);
        }
        // If GUARD_GRANULE is unset, no pvmfw ran → MMIO guard not active →
        // regular KVM already allows the access via stage-2.
        Ok(())
    }
}

fn dma_unshare_pages(paddr: usize, size: usize) {
    let page_size = 0x1000;
    let pages = size / page_size;
    for i in 0..pages {
        let (ret0, _ret1) = psci_hvc_call(
            ARM_SMCCC_VENDOR_HYP_KVM_MEM_UNSHARE_FUNC_ID,
            paddr + page_size * i,
            1,
            0,
        );
        if ret0 != 0 {
            log::warn!(
                "[virtio hal impl] cannot unshare 0x{:x}",
                paddr + page_size * i
            );
        }
    }
}

fn dma_share_pages(paddr: usize, size: usize) {
    let page_size = 0x1000;
    let pages = size / page_size;
    for i in 0..pages {
        let (ret0, _ret1) = psci_hvc_call(
            ARM_SMCCC_VENDOR_HYP_KVM_MEM_SHARE_FUNC_ID,
            paddr + page_size * i,
            1,
            0,
        );
        if ret0 != 0 {
            log::warn!(
                "[virtio hal impl] cannot share 0x{:x}",
                paddr + page_size * i
            );
        }
    }
}
