// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cache maintenance operations for x86_64.
//!
//! x86_64 maintains instruction cache coherency in hardware, so all icache
//! flush operations are no-ops.

use memaddr::VirtAddr;

/// Return the CLFLUSH stride, or None when the processor lacks CLFLUSH.
pub fn data_cache_line_size() -> Option<usize> {
    let info = core::arch::x86_64::__cpuid(1);
    let size = ((info.ebx >> 8) & 0xff) as usize * 8;
    (info.edx & (1 << 19) != 0 && size.is_power_of_two()).then_some(size)
}

/// Clean and invalidate complete cache lines before changing memory attributes.
///
/// # Safety
/// CLFLUSH is available; both endpoints are aligned to its reported stride.
/// The nonempty range is mapped live RAM without overflow, with CPU aliases and
/// device accesses stopped until maintenance and the attribute change complete.
pub unsafe fn clean_invalidate_dcache_range_to_poc(start: VirtAddr, size: usize) {
    if let Some(line_size) = data_cache_line_size() {
        for address in (start.as_usize()..start.as_usize() + size).step_by(line_size) {
            // SAFETY: caller retains exclusively owned mapped whole cache lines;
            // CPUID established CLFLUSH availability and the exact instruction stride.
            unsafe {
                core::arch::asm!("clflush [{address}]", address = in(reg) address, options(nostack))
            };
        }
        // SAFETY: MFENCE completes preceding CLFLUSH operations before the caller proceeds.
        unsafe { core::arch::asm!("mfence", options(nostack)) };
    }
}

/// Local icache flush — no-op on x86_64.
#[inline]
pub fn flush_icache_all_local() {}

/// What a remote CPU must execute when receiving an icache-flush IPI.
///
/// x86_64 maintains icache coherency in hardware, so this is a no-op.
#[inline]
pub fn flush_icache_remote() {}

/// Flushes the entire instruction cache (no-op on x86_64).
#[inline]
pub fn flush_icache_all() {}

/// Flushes the instruction cache for the given virtual address range (no-op on x86_64).
#[inline]
pub fn flush_icache_range(_start: VirtAddr, _size: usize) {}

/// Orders prior CPU stores before a device reads the same memory via DMA.
///
/// x86_64 is cache-coherent for device DMA and maintains strong store
/// ordering, so no barrier is required. The explicit no-op keeps driver call
/// sites portable across architectures — the LoongArch64 implementation
/// executes `dbar 0`.
#[inline]
pub fn dma_read_barrier() {}
