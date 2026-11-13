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
/// This function allocates a block through the existing DMA32 PageRef allocator. The
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

/// Frees coherent memory previously allocated.
///
/// This function releases the memory block that was previously allocated and
/// marked as coherent. It ensures proper deallocation and management of resources
/// associated with the memory block. Requires sleepable task context.
/// An invalid context, failed platform release, or unconfirmed mapping
/// restoration is logged and retains the pages rather than reusing them.
///
/// - `dma_info`: An instance of [`DmaInfo`] containing the details of the memory
///   block to be freed, such as its starting address and size.
///
/// # Safety
///
/// `dma` must come from a prior successful [`allocate_dma_memory`] call, and
/// `layout` must exactly match that allocation.
#[track_caller]
pub unsafe fn deallocate_dma_memory(dma: DmaInfo, layout: Layout) {
    // SAFETY: forwards the caller's matching deallocation contract to the global DMA allocator.
    unsafe { dma::deallocate_dma_memory(dma, layout) }
}

/// Direction for a temporary DMA mapping of an existing buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// Copying runs outside the metadata lock while the bounce
/// allocation remains exclusively owned. Keeping this
/// behind `kdma` lets future implementations restore direct-map optimizations
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
    _direction: DmaDirection,
) -> AllocResult<DmaInfo> {
    let reservation = ALLOCATOR.lock().reserve_mapping(buffer.len());
    let (dma, mut mapping) = match reservation {
        Err(AllocError::NotInitialized) => {
            // The metadata guard has ended. Preparation admits only sleepable
            // task context, so an IRQ caller never enters the mapping slow path.
            dma::ensure_bounce_pool()?;
            ALLOCATOR.lock().reserve_mapping(buffer.len())?
        }
        result => result?,
    };
    // Stage every direction so cancellation cannot expose stale pool bytes.
    let buffer =
        NonNull::slice_from_raw_parts(buffer.cast::<core::mem::MaybeUninit<u8>>(), buffer.len());
    // SAFETY: the caller retains this buffer exclusively until unmap; the token
    // owns distinct staging storage. MaybeUninit preserves RX bytes without
    // requiring the device's destination to be initialized before submission.
    mapping.copy_from_slice(unsafe { buffer.as_ref() });
    ALLOCATOR.lock().activate_mapping(dma.bus_addr, mapping);
    Ok(dma)
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
    let mapping = ALLOCATOR.lock().take_mapping(dma_addr, buffer.len());
    if direction == DmaDirection::DriverToDevice {
        mapping.sync_for_cpu(None);
    } else {
        let mut buffer = NonNull::slice_from_raw_parts(
            buffer.cast::<core::mem::MaybeUninit<u8>>(),
            buffer.len(),
        );
        // SAFETY: the caller has stopped device access and exclusively owns the
        // original buffer. The removed mapping retains its distinct extent.
        mapping.sync_for_cpu(Some(unsafe { buffer.as_mut() }));
    }
    ALLOCATOR.lock().recycle_bounce_buffer(mapping);
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
    // SAFETY: forwards the caller's never-published mapping contract.
    unsafe { ALLOCATOR.lock().cancel_dma_mapping(dma_addr, len) }
}

/// A bus memory address.
///
/// It's a wrapper type around an [`u64`].
#[repr(transparent)]
#[derive(Copy, Clone, Default, Ord, PartialOrd, Eq, PartialEq)]
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
/// For coherent allocations the original address/Layout pair carries one exact
/// PageRef transfer; copying this record never acquires another reference.
/// Streaming mappings instead retain their pool extent in DMA bookkeeping.
#[derive(Debug, Clone, Copy)]
pub struct DmaInfo {
    /// The address at which the CPU accesses this memory region. This address
    /// is a virtual memory address used by the CPU to access memory.
    pub cpu_addr: NonNull<u8>,
    /// Represents the physical address of this memory region on the bus. The DMA
    /// controller uses this address to directly access memory.
    pub bus_addr: DmaBusAddress,
}
