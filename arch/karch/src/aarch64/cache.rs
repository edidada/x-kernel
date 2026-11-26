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
    // SAFETY: the caller passes `vaddr` from a live, mapped kernel range
    // (boot/firmware state it owns). Aligning down to a cache-line boundary
    // stays in the same mapped page, so `dc cvac` addresses a mapped VA.
    // `dsb sy` waits for completion before the caller releases another
    // observer, such as a secondary CPU running with the MMU off.
    unsafe { asm!("dc cvac, {0:x}; dsb sy", in(reg) line_vaddr) };
}

/// Cleans every data cache line covering `[start, start + size)` to the Point
/// of Coherency.
#[inline]
pub fn clean_dcache_range_to_poc(start: VirtAddr, size: usize) {
    let addr = start.as_usize() & !(CACHE_LINE - 1);
    let end = (start.as_usize() + size + CACHE_LINE - 1) & !(CACHE_LINE - 1);
    for va in (addr..end).step_by(CACHE_LINE) {
        // SAFETY: the caller passes a live, mapped kernel range (for example
        // the buffer owned by `DmaBuf::sync`, whose range is clamped to the
        // allocation, or by a `DmaOp::flush_cache` caller). Cache-line
        // rounding only extends into the same mapped pages, so every `va`
        // here is mapped and `dc cvac` cannot fault. It makes MMU-off or
        // non-coherent readers observe this CPU's writes from PoC.
        unsafe { asm!("dc cvac, {0:x}", in(reg) va) };
    }
    // SAFETY: `dsb sy` waits until all preceding clean operations reach PoC.
    unsafe { asm!("dsb sy") };
}

/// Invalidates every data cache line covering `[start, start + size)` to the
/// Point of Coherency.
///
/// The counterpart of [`clean_dcache_range_to_poc`], for the other DMA
/// direction: discards this CPU's copy so a later read observes what a
/// non-coherent device wrote.
///
/// # Caller contract
///
/// `dc ivac` drops dirty lines *without* writing them back. Invalidating a
/// range this CPU has written since its last clean silently loses those
/// writes, so a bidirectional buffer must be cleaned before it is
/// invalidated. Ranges that the device is known not to have written are
/// better cleaned alone.
#[inline]
pub fn invalidate_dcache_range_to_poc(start: VirtAddr, size: usize) {
    let addr = start.as_usize() & !(CACHE_LINE - 1);
    let end = (start.as_usize() + size + CACHE_LINE - 1) & !(CACHE_LINE - 1);
    for va in (addr..end).step_by(CACHE_LINE) {
        // SAFETY: the caller passes a live, mapped kernel range (for example
        // the buffer owned by `DmaBuf::sync`, whose range is clamped to the
        // allocation, or by a `DmaOp::invalidate_cache` caller). Cache-line
        // rounding only extends into the same mapped pages, so every `va`
        // here is mapped and `dc ivac` cannot fault. It discards this CPU's
        // copy so the next read misses to PoC; the dirty-line hazard is the
        // caller contract documented above.
        unsafe { asm!("dc ivac, {0:x}", in(reg) va) };
    }
    // SAFETY: `dsb sy` waits until all preceding invalidations have completed
    // before the caller reads the range.
    unsafe { asm!("dsb sy") };
}

/// Orders prior CPU stores before a device reads the same memory via DMA.
///
/// The AArch64 platforms in this configuration are cache-coherent for device
/// DMA, so no barrier is required. The explicit no-op keeps driver call sites
/// portable across architectures — the LoongArch64 implementation executes
/// `dbar 0`.
///
/// This says nothing about a *non-coherent* master such as the RK3588 NPU:
/// there the ordering guarantee comes from the clean in
/// [`dma_clean_range`] and its trailing `dsb sy`.
#[inline]
pub fn dma_read_barrier() {}

/// Cleans `[start, start + size)` so a non-coherent device observes this CPU's
/// writes.
///
/// The architecture-neutral spelling of [`clean_dcache_range_to_poc`], for
/// callers that only care about the DMA direction — device-buffer code, which
/// should not have to know whether the host maintains caches at all. On a
/// coherent architecture this is a no-op, exactly like [`dma_read_barrier`].
#[inline]
pub fn dma_clean_range(start: VirtAddr, size: usize) {
    clean_dcache_range_to_poc(start, size);
}

/// Invalidates `[start, start + size)` so the CPU observes a non-coherent
/// device's writes.
///
/// The counterpart of [`dma_clean_range`], and inherits its architecture
/// neutrality. The dirty-line hazard documented on
/// [`invalidate_dcache_range_to_poc`] applies unchanged: clean before
/// invalidating a range this CPU has written.
#[inline]
pub fn dma_invalidate_range(start: VirtAddr, size: usize) {
    invalidate_dcache_range_to_poc(start, size);
}
