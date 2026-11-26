// NOTICE: Portions of this file may appear structurally similar to code in other open source projects (e.g., libsql/libsql, optuna/kurobako, novifinancial/winterfell),
// but the semantics and implementation intent are entirely different. Any resemblance is coincidental and does not indicate code reuse or derivation.
//
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DMA allocation helpers and bus address types.
//!
//! `kdma` is the kernel's DMA memory layer: [`allocate_dma_memory`] and
//! [`deallocate_dma_memory`] manage coherent, device-reachable memory;
//! [`map_dma_buffer`] / [`unmap_dma_buffer`] stage ordinary buffers
//! through a coherent bounce pool for temporary transactions;
//! [`cancel_dma_mapping`] releases a mapping that was never published. [`p2b`]
//! / [`b2p`] convert between physical and bus addresses. See
//! `docs/design.md` for the full boundary contracts.
//!
//! # Example
//!
//! Map a driver-owned buffer for a device read (DeviceToDriver), then
//! unmap it once the device is done. Kernel code only; this crate cannot
//! compile in a host doctest environment, so the example is `ignore`.
//!
//! ```ignore
//! use core::ptr::NonNull;
//! use kdma::{map_dma_buffer, unmap_dma_buffer, DmaDirection};
//!
//! # fn stage(buffer: NonNull<[u8]>) -> Result<(), alloc_engine::AllocError> {
//! // 1. Stage the driver-owned buffer through the coherent bounce pool.
//! let dma = unsafe { map_dma_buffer(buffer, DmaDirection::DeviceToDriver) }?;
//!
//! // 2. Program the device with dma.bus_addr and wait for completion.
//! // device.submit(dma.bus_addr, buffer.len());
//!
//! // 3. Unmap exactly once with the same address and buffer.
//! unsafe { unmap_dma_buffer(dma.bus_addr, buffer, DmaDirection::DeviceToDriver) };
//! # Ok(())
//! # }
//! ```
//!
//! For persistent device-reachable memory (descriptor rings, shared
//! queues), use [`allocate_dma_memory`] instead, which returns a
//! [`DmaInfo`] whose bus address stays valid until
//! [`deallocate_dma_memory`].
#![no_std]

extern crate alloc;

mod bounce_pool;
mod dma;

use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::{AllocError, AllocResult};
use memaddr::PhysAddr;

use self::dma::ALLOCATOR;
#[cfg(unittest)]
pub use self::dma::DmaTestStats;

/// Returns a read-only snapshot of in-use DMA resources.
///
/// This exists only for unit-test baselines. It does not change allocation or
/// release paths.
#[cfg(unittest)]
pub fn dma_test_stats() -> DmaTestStats {
    ALLOCATOR.lock().test_stats()
}

/// Attempts to prepare the fixed streaming pool before driver discovery.
///
/// Must run in task context with local IRQs enabled, after MM and CPU services
/// are initialized. Subsequent map/unmap operations never modify page tables.
/// Repeated calls reuse the prepared pool.
///
/// # Errors
/// Returns an allocation/preparation error, including `InvalidInput` for an
/// IRQ-off or interrupt/BH caller. The host may continue booting on failure.
/// A later streaming map in sleepable task context retries preparation once;
/// IRQ/BH mappings fail while backing is unavailable. Unrelated drivers do not
/// require a prepared streaming pool.
pub fn init_streaming_pool() -> AllocResult<()> {
    dma::ensure_bounce_pool()
}

/// Converts a physical address to a bus address.
///
/// It assumes that there is a linear mapping with the offset
/// [`kbuild_config::PHYS_BUS_OFFSET`], that maps all the physical memory
/// to the virtual space at the address plus the offset. So we have
/// `baddr = paddr + PHYS_BUS_OFFSET`.
#[inline]
pub const fn p2b(paddr: PhysAddr) -> DmaBusAddress {
    DmaBusAddress::new((paddr.as_usize() + kbuild_config::PHYS_BUS_OFFSET) as u64)
}

/// Converts a bus address back to a physical address.
#[inline]
pub fn b2p(baddr: DmaBusAddress) -> PhysAddr {
    ((baddr.as_u64() as usize) - kbuild_config::PHYS_BUS_OFFSET).into()
}

/// Allocates **coherent** memory that meets Direct Memory Access (DMA)
/// requirements.
///
/// This function allocates a block through the existing DMA32 allocator. The
/// memory pages must be contiguous, undivided, and have consistent read and
/// write access.
///
/// - `layout`: The memory layout, which describes the size and alignment
///   requirements of the requested memory.
///
/// Returns an [`DmaInfo`] structure containing details about the allocated
/// memory, such as the starting address and size. If it's not possible to
/// allocate memory meeting the criteria, returns [`None`].
///
/// Requires sleepable task context. Page allocation, mapping
/// transitions and platform hooks run outside the DMA metadata lock. Callers
/// must not hold locks required by remote CPUs to complete a TLB shootdown.
///
/// # Errors
///
/// Returns `AllocError::InvalidInput` for non-sleepable callers,
/// and `AllocError::NoMemory` when pages, mapping updates or platform preparation
/// fail. Failed preparation retains private pages rather than publishing them.
///
/// # Safety
///
/// The caller must free the returned allocation exactly once with
/// [`deallocate_dma_memory`] using the same `layout`, and must only expose the
/// returned bus address to devices that are allowed to DMA that buffer.
#[track_caller]
pub unsafe fn allocate_dma_memory(layout: Layout) -> AllocResult<DmaInfo> {
    dma::allocate_dma_memory(layout)
}

/// Allocates contiguous DMA memory whose CPU mapping stays **cacheable**.
///
/// Same pages and bus address as [`allocate_dma_memory`], minus the
/// non-cacheable re-protection. Use this when the buffer has to be shared with
/// userspace as ordinary memory — a dma-buf from the heap, for instance — and
/// the caller performs cache maintenance explicitly.
///
/// Requires sleepable task context with local IRQs enabled. Platform preparation
/// runs outside the DMA metadata lock, as in [`allocate_dma_memory`].
///
/// # Errors
///
/// Same as [`allocate_dma_memory`].
///
/// # Safety
///
/// The caller must free the returned allocation exactly once with
/// [`deallocate_dma_memory`] using the same `layout`, must clean the buffer
/// before any device reads it and invalidate it before reading what a device
/// wrote, and must only expose the bus address to devices allowed to DMA it.
#[track_caller]
pub unsafe fn allocate_dma_memory_cacheable(layout: Layout) -> AllocResult<DmaInfo> {
    // SAFETY: the safe-typed `Layout` establishes size and alignment validity.
    // The remaining preconditions are this unsafe function's own contract and
    // are established by its caller: release the returned allocation exactly
    // once with this same layout, clean before a device reads and invalidate
    // before the CPU reads device writes, and expose the bus address only to
    // devices permitted to DMA that range. The callee allocates the pages and
    // leaves their Normal mapping in place; it performs no cache maintenance
    // and cannot restrict which devices receive the address.
    unsafe { dma::alloc_dma_pages_cacheable(layout) }
}

/// Frees memory from [`allocate_dma_memory`] or [`allocate_dma_memory_cacheable`].
///
/// Requires sleepable task context.
/// An invalid context, failed platform release, or mapping restoration error
/// is logged and retains the pages rather than reusing them. TLB invalidation
/// itself completes synchronously before restoration returns.
///
/// - `dma_info`: An instance of [`DmaInfo`] containing the details of the memory
///   block to be freed, such as its starting address and size.
///
/// # Safety
///
/// `dma` must come from a prior successful [`allocate_dma_memory`] or
/// [`allocate_dma_memory_cacheable`] call, and `layout` must exactly match that
/// allocation. The allocation must still be live, device access must have
/// stopped, and it must be released exactly once.
#[track_caller]
pub unsafe fn deallocate_dma_memory(dma: DmaInfo, layout: Layout) {
    // SAFETY: the caller supplies the live DmaInfo from one of the allocation
    // functions and its exact Layout, releases it once, and has stopped device
    // access. These establish the native allocator's provenance and exclusive-release contract.
    unsafe { dma::deallocate_dma_memory(dma, layout) }
}

/// Direction for a temporary DMA mapping of an existing buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaDirection {
    /// The device reads data the CPU produced (e.g. a TX descriptor ring).
    DriverToDevice,
    /// The device produces data the CPU reads (e.g. an RX descriptor ring).
    DeviceToDriver,
    /// The buffer is both read and written by the device.
    Bidirectional,
}

/// Maps an existing buffer for temporary DMA access.
///
/// The current backend always stages the mapping through pooled coherent bounce
/// memory and copies the original bytes for every direction. The host must
/// attempt pool preparation with [`init_streaming_pool`] before driver discovery;
/// mapping returns an error if the pool is unavailable.
/// If backing is missing, a sleepable task retries preparation once outside
/// the metadata lock. IRQ/BH and IRQ-off mappings return an error instead of
/// entering MM or waiting for another CPU. A prepared pool never grows.
/// Keeping this behind `kdma` lets future implementations restore direct-map optimizations
/// with stricter page-granularity safety checks without changing driver code.
///
/// # Errors
///
/// Returns `AllocError::InvalidInput` for a zero-length buffer or a bus
/// address that is already actively mapped, and
/// `AllocError::NoMemory` when the bounce pool cannot satisfy the
/// request or task-context pool preparation fails. Returns `AllocError::InvalidInput`
/// if preparation is required in a non-sleepable context. After preparation,
/// mapping/unmapping supports ordinary IRQ and BH contexts, but not NMI.
///
/// # Safety
///
/// `buffer` must remain live and exclusively owned until it is later passed to
/// [`unmap_dma_buffer`]. The caller must not let the device keep using the
/// returned bus address after unmapping.
pub unsafe fn map_dma_buffer(
    buffer: NonNull<[u8]>,
    direction: DmaDirection,
) -> AllocResult<DmaInfo> {
    // SAFETY: the caller keeps the buffer exclusively owned through unmap.
    let result = unsafe { ALLOCATOR.lock().map_dma_buffer(buffer, direction) };
    if matches!(result, Err(AllocError::NotInitialized)) {
        // The metadata guard has ended. Only a sleepable caller may prepare
        // coherent pool backing, which changes mappings and waits for TLBs.
        dma::ensure_bounce_pool()?;
        // SAFETY: the failed attempt did not publish or retain this buffer.
        return unsafe { ALLOCATOR.lock().map_dma_buffer(buffer, direction) };
    }
    result
}

/// Unmaps a previously mapped DMA buffer and synchronizes data back if needed.
///
/// # Safety
///
/// `dma_addr` must be the still-active bus address previously returned by
/// [`map_dma_buffer`] for the same `buffer`, and unmapping must happen exactly
/// once for that mapping.
/// # Panics
///
/// Panics when `dma_addr` is not an active mapping or when `buffer`'s
/// length does not match the length recorded at [`map_dma_buffer`] time;
/// both are caller contract violations, not runtime conditions.
///
/// # Example
///
/// The unmap must repeat the exact address, buffer, and length from the
/// matching [`map_dma_buffer`] call — see the crate-level
/// [example](crate#example) for the full map → device → unmap sequence.
pub unsafe fn unmap_dma_buffer(
    dma_addr: DmaBusAddress,
    buffer: NonNull<[u8]>,
    direction: DmaDirection,
) {
    // SAFETY: the caller provides the exact active mapping and stopped DMA.
    unsafe {
        ALLOCATOR
            .lock()
            .unmap_dma_buffer(dma_addr, buffer, direction)
    }
}

/// Releases a mapping that was never handed to a device.
///
/// Unlike [`unmap_dma_buffer`], this performs no copy-back: mapping stages the
/// original bytes for both read and write directions, and only an actual
/// device transfer may change them. Use this for rollback when the mapping
/// cannot be published, so a failed device-read mapping cannot clobber the
/// caller's buffer.
///
/// # Safety
///
/// `dma_addr` must be the still-active address previously returned by
/// [`map_dma_buffer`], `len` must equal the mapped length, and the mapping must
/// be cancelled exactly once. No device may have accessed the address.
/// # Panics
///
/// Panics when `dma_addr` is not active or `len` does not match; both are
/// caller contract violations.
pub unsafe fn cancel_dma_mapping(dma_addr: DmaBusAddress, len: usize) {
    // SAFETY: the caller supplies the still-active address from map_dma_buffer
    // with its original length, cancels it once, and guarantees no device access.
    // The allocator lock serializes consuming that exact unpublished mapping.
    unsafe { ALLOCATOR.lock().cancel_dma_mapping(dma_addr, len) }
}

/// A bus memory address.
///
/// It's a wrapper type around an [`u64`].
#[repr(transparent)]
#[derive(Clone, Copy, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct DmaBusAddress(u64);

impl DmaBusAddress {
    /// Converts an [`u64`] to a physical address.
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    /// Converts the address to an [`u64`].
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for DmaBusAddress {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl core::fmt::Debug for DmaBusAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("DmaBusAddress")
            .field(&format_args!("{:#X}", self.0))
            .finish()
    }
}

/// DMA addresses, not a clonable ownership handle.
///
/// Coherent allocations are released once with their original address/Layout
/// pair. Streaming mappings retain their pool extent in DMA bookkeeping.
#[derive(Clone, Copy, Debug)]
pub struct DmaInfo {
    /// The address at which the CPU accesses this memory region. This address
    /// is a virtual memory address used by the CPU to access memory.
    pub cpu_addr: NonNull<u8>,
    /// Represents the physical address of this memory region on the bus. The DMA
    /// controller uses this address to directly access memory.
    pub bus_addr: DmaBusAddress,
}
