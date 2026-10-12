// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cache maintenance operations for AArch64.
//!
//! AArch64 `ic ivau` / `ic iallu` broadcast to the inner shareable domain via
//! hardware, so icache entries are invalidated on all PEs. However, only an
//! `isb` on a given PE flushes that PE's pipeline of stale prefetched
//! instructions. We therefore send an IPI (via `kiface` → `kipi`) so
//! every PE executes `isb`.

use core::arch::asm;

use memaddr::VirtAddr;

const CACHE_LINE: usize = 64;

/// Local icache flush and pipeline sync — does not affect other PEs.
#[inline]
pub fn flush_icache_all_local() {
    // SAFETY: `ic iallu`, `dsb`, and `isb` are privileged cache-maintenance
    // and barrier instructions executed on the current CPU only.
    unsafe { asm!("ic iallu; dsb sy; isb") };
}

/// What a remote PE must execute when receiving an icache-flush IPI.
///
/// The icache invalidation already broadcast to all PEs via hardware (`ic ivau`
/// / `ic iallu`). The remote PE only needs an `isb` to flush stale instructions
/// from its pipeline. Called from the `kipi` IPI callback.
#[inline]
pub fn flush_icache_remote() {
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);
}

/// Flushes the entire instruction cache and synchronises pipelines on all PEs.
///
/// Executes `ic iallu; dsb sy; isb` locally, then sends IPIs so other PEs
/// execute `isb` to flush their pipelines.
#[inline]
pub fn flush_icache_all() {
    flush_icache_all_local();
    if kcpu_id_map::nr_cpus() > 1 {
        crate::flush_icache_others();
    }
}

/// Flushes the instruction cache for the given virtual address range on all PEs.
///
/// Cleans data cache and invalidates instruction cache per cache line for
/// `[start, start + size)`; the `ic ivau` broadcast invalidates the icache on
/// all PEs. An IPI is then sent so other PEs execute `isb`.
#[inline]
pub fn flush_icache_range(start: VirtAddr, size: usize) {
    let addr = start.as_usize() & !(CACHE_LINE - 1);
    let end = (start.as_usize() + size + CACHE_LINE - 1) & !(CACHE_LINE - 1);
    for va in (addr..end).step_by(CACHE_LINE) {
        // SAFETY: `dc cvau` cleans by VA to PoU,
        // matching Linux arm64 caches_clean_inval_pou() semantics. `ic ivau`
        // invalidates I-cache by VA to PoU. Both instructions are standard
        // ARM instructions, safe at all privilege levels.
        unsafe { asm!("dc cvau, {0:x}", in(reg) va) };
        // SAFETY: At the kernel exception level IC IVAU is available; va names
        // the same cache line just cleaned to PoU for instruction execution.
        unsafe { asm!("ic ivau, {0:x}", in(reg) va) };
    }
    // Completing IC IVAU requires a DSB covering both reads and writes;
    // a store-only DSB ISHST cannot provide this completion guarantee.
    aarch64_cpu::asm::barrier::dsb(aarch64_cpu::asm::barrier::ISH);
    aarch64_cpu::asm::barrier::isb(aarch64_cpu::asm::barrier::SY);

    if kcpu_id_map::nr_cpus() > 1 {
        crate::flush_icache_others();
    }
}

/// Cleans the data cache line covering `vaddr` to the Point of Coherency.
///
/// This is the correct primitive when the current CPU wrote shared boot or
/// firmware-visible state through a cacheable mapping and another observer may
/// read it before joining the normal coherent MMU-on world.
#[inline]
pub fn clean_dcache_line_to_poc(vaddr: VirtAddr) {
    let line_vaddr = vaddr.as_usize() & !(CACHE_LINE - 1);
    // SAFETY: `dc cvac` cleans the cache line containing `line_vaddr` to the
    // Point of Coherency. The address is aligned to a cache-line boundary, and
    // `dsb sy` waits for completion before the caller releases another
    // observer, such as a secondary CPU running with the MMU off.
    unsafe { asm!("dc cvac, {0:x}; dsb sy", in(reg) line_vaddr) };
}

/// Cleans every data cache line covering `[start, start + size)` to the Point
/// of Coherency.
#[inline]
pub fn clean_dcache_range_to_poc(start: VirtAddr, size: usize) {
    let line_size = data_cache_line_size();
    let addr = start.as_usize() & !(line_size - 1);
    let end = (start.as_usize() + size + line_size - 1) & !(line_size - 1);
    for va in (addr..end).step_by(line_size) {
        // SAFETY: `dc cvac` is issued for each cache line covering the
        // requested range so MMU-off or non-coherent readers observe the
        // current CPU's writes from PoC.
        unsafe { asm!("dc cvac, {0:x}", in(reg) va) };
    }
    // SAFETY: `dsb sy` waits until all preceding clean operations reach PoC.
    unsafe { asm!("dsb sy") };
}

/// Return the data-cache maintenance stride reported by CTR_EL0.DminLine.
pub fn data_cache_line_size() -> usize {
    let ctr: u64;
    // SAFETY: CTR_EL0 is readable at the kernel exception level and has no side effects.
    unsafe { asm!("mrs {ctr}, ctr_el0", ctr = out(reg) ctr, options(nomem, nostack)) };
    4 << ((ctr >> 16) & 0xf)
}

/// Invalidate an exclusively owned range of complete cache lines to PoC.
///
/// # Safety
/// Both endpoints are aligned to data_cache_line_size(), size is nonzero, and
/// the range is mapped live RAM without address overflow. CPU aliases and DMA
/// accesses have stopped; dirty CPU data may be discarded.
pub unsafe fn invalidate_dcache_range_to_poc(start: VirtAddr, size: usize) {
    let end = start.as_usize() + size;
    for address in (start.as_usize()..end).step_by(data_cache_line_size()) {
        // SAFETY: caller supplies exclusively owned whole live cache lines and
        // authorizes discarding CPU contents after stopping CPU and DMA accesses.
        unsafe { asm!("dc ivac, {address}", address = in(reg) address, options(nostack)) };
    }
    // SAFETY: DSB SY completes every preceding invalidation before ownership returns.
    unsafe { asm!("dsb sy", options(nostack)) };
}

/// Clean and invalidate an exclusively owned range of complete cache lines to PoC.
///
/// # Safety
/// Both endpoints are aligned to data_cache_line_size(), size is nonzero, and
/// the range is mapped live RAM without address overflow. CPU aliases and DMA
/// accesses have stopped for the duration of maintenance.
pub unsafe fn clean_invalidate_dcache_range_to_poc(start: VirtAddr, size: usize) {
    let end = start.as_usize() + size;
    for address in (start.as_usize()..end).step_by(data_cache_line_size()) {
        // SAFETY: caller supplies exclusively owned live cache lines with both
        // owners stopped; CIVAC preserves dirty bytes before invalidating them.
        unsafe { asm!("dc civac, {address}", address = in(reg) address, options(nostack)) };
    }
    // SAFETY: DSB SY completes maintenance before a device or attribute change proceeds.
    unsafe { asm!("dsb sy", options(nostack)) };
}

/// Orders prior CPU stores before a device reads the same memory via DMA.
///
/// The AArch64 platforms in this configuration are cache-coherent for device
/// DMA, so no barrier is required. The explicit no-op keeps driver call sites
/// portable across architectures — the LoongArch64 implementation executes
/// `dbar 0`.
#[inline]
pub fn dma_read_barrier() {}
