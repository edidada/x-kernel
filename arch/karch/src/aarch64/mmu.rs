// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MMU/page table operations for AArch64.

use aarch64_cpu::registers::{Readable, TTBR0_EL1, Writeable};
use memaddr::PhysAddr;

/// Hardware-ready page-table root value.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HwPageTableRoot(usize);

impl HwPageTableRoot {
    #[inline]
    pub const fn new(raw: usize) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl From<PhysAddr> for HwPageTableRoot {
    fn from(root_paddr: PhysAddr) -> Self {
        HwPageTableRoot::new(root_paddr.as_usize())
    }
}

/// Reads the current page table root register for kernel space.
///
/// When the `arm-el2` feature is enabled, reads `TTBR0_EL2`; otherwise
/// reads `TTBR1_EL1`.
///
/// Returns the hardware-ready page table root value.
#[inline]
pub fn read_kernel_page_table() -> HwPageTableRoot {
    let pt_root_reg: usize;

    #[cfg(not(feature = "arm-el2"))]
    {
        use aarch64_cpu::registers::TTBR1_EL1;
        pt_root_reg = TTBR1_EL1.get() as usize;
    }

    #[cfg(feature = "arm-el2")]
    {
        use aarch64_cpu::registers::TTBR0_EL2;
        pt_root_reg = TTBR0_EL2.get() as usize;
    }

    HwPageTableRoot::new(pt_root_reg)
}

/// Reads the current page table root register for user space (`TTBR0_EL1`).
///
/// Returns the hardware-ready page table root value.
#[inline]
pub fn read_user_page_table() -> HwPageTableRoot {
    let val = TTBR0_EL1.get();
    HwPageTableRoot::new(val as usize)
}

/// Writes the register to update the current page table root for kernel space.
///
/// When the `arm-el2` feature is enabled, writes `TTBR0_EL2`; otherwise
/// writes `TTBR1_EL1`.
///
/// An ISB is issued after the write to synchronise the context change, so
/// that subsequent instructions (including TLBI) operate under the new
/// translation regime.
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_kernel_page_table(root: HwPageTableRoot) {
    #[cfg(not(feature = "arm-el2"))]
    {
        use aarch64_cpu::registers::TTBR1_EL1;
        TTBR1_EL1.set(root.as_usize() as _);
    }

    #[cfg(feature = "arm-el2")]
    {
        use aarch64_cpu::registers::TTBR0_EL2;
        TTBR0_EL2.set(root.as_usize() as _);
    }

    // ISB synchronises the TTBR write so that subsequent instructions
    // (including TLBI) see the new translation regime.  Without this,
    // a following TLBI may apply to the old TTBR.
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
}

/// Writes the register to update the current page table root for user space
/// (`TTBR0_EL1`).
///
/// An ISB is issued after the write to synchronise the context change, so
/// that subsequent instructions (including TLBI) operate under the new
/// translation regime.
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_user_page_table(root: HwPageTableRoot) {
    TTBR0_EL1.set(root.as_usize() as _);
    // ISB synchronises the TTBR write so that subsequent instructions
    // (including TLBI) see the new translation regime.
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
}

// --- Raw kernel-page-table walk for guard pages (see `TaskStack`) ---------

/// AArch64 kernel linear-map offset: kernel VA = phys + this.
const LINMAP_OFF: usize = 0xFFFF_0000_0000_0000;
/// Output address bits [47:12] of a PTE / TTBR1 base address.
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

#[inline]
fn phys_to_kvirt(phys: usize) -> usize {
    phys.wrapping_add(LINMAP_OFF)
}

/// Walks the kernel page table (TTBR1) to the 4K leaf
/// for `va`, writes the leaf PTE to 0 (unmapping it), and broadcasts a TLB
/// invalidation for `va` (Inner-Shareable, so all cores). Returns the saved
/// PTE so it can be restored by [`remap_kernel_4k`].
///
/// Returns `None` if `va` is not a 4K-mapped leaf (unmapped, or covered by a
/// 1G/2M block mapping). Self-contained: does not touch `memspace`.
pub fn unmap_kernel_4k(va: usize) -> Option<u64> {
    // SAFETY: we only read/write PTEs of the live kernel page table and issue
    // the architected TLBI sequence. IRQs are expected to be off around the
    // unmap/remap pair to keep the PTE update atomic w.r.t. faults.
    unsafe {
        let mut table = (read_kernel_page_table().as_usize() as u64 & ADDR_MASK) as usize;
        // L0..L2 must be table descriptors.
        for sh in [39usize, 30, 21] {
            let idx = (va >> sh) & 0x1FF;
            let pte = ((phys_to_kvirt(table) + idx * 8) as *const u64).read_volatile();
            if pte & 0b11 != 0b11 {
                return None; // block or invalid -> not a 4K leaf path
            }
            table = (pte & ADDR_MASK) as usize;
        }
        // L3: 4K page leaf.
        let idx = (va >> 12) & 0x1FF;
        let pte_ptr = (phys_to_kvirt(table) + idx * 8) as *mut u64;
        let pte = pte_ptr.read_volatile();
        if pte & 0b1 == 0 {
            return None; // not mapped
        }
        pte_ptr.write_volatile(0);
        super::tlb::flush_tlb(Some(memaddr::VirtAddr::from_usize(va)));
        Some(pte)
    }
}

/// Restores the PTE cleared by [`unmap_kernel_4k`].
pub fn remap_kernel_4k(va: usize, saved_pte: u64) {
    // SAFETY: see `unmap_kernel_4k`; restores the exact prior leaf PTE.
    unsafe {
        let mut table = (read_kernel_page_table().as_usize() as u64 & ADDR_MASK) as usize;
        for sh in [39usize, 30, 21] {
            let idx = (va >> sh) & 0x1FF;
            let pte = ((phys_to_kvirt(table) + idx * 8) as *const u64).read_volatile();
            if pte & 0b11 != 0b11 {
                return; // cannot reach L3
            }
            table = (pte & ADDR_MASK) as usize;
        }
        let idx = (va >> 12) & 0x1FF;
        let pte_ptr = (phys_to_kvirt(table) + idx * 8) as *mut u64;
        pte_ptr.write_volatile(saved_pte);
        super::tlb::flush_tlb(Some(memaddr::VirtAddr::from_usize(va)));
    }
}
