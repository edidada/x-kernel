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
//! through a shared coherent bounce pool for temporary transactions.
//! Every operation receives a [`DmaDeviceContext`] selected by device probe.
//! Device masks are checked against complete bus intervals; [`p2b`] / [`b2p`]
//! describe the platform conversion. See
//! `docs/design.md` for the full boundary contracts.
//!
//! # Example
//!
//! Map an initialized driver-owned receive buffer for device writes, then
//! unmap it once the device is done. Kernel code only; this crate cannot
//! compile in a host doctest environment, so the example is `ignore`.
//!
//! ```ignore
//! use core::ptr::NonNull;
//! use kdma::{map_dma_buffer, unmap_dma_buffer, DmaDirection, DmaDeviceContext};
//!
//! # fn stage(device: &DmaDeviceContext, buffer: NonNull<[u8]>) -> Result<(), alloc_engine::AllocError> {
//! // 1. Stage the driver-owned buffer through the coherent bounce pool.
//! let dma = unsafe { map_dma_buffer(device, buffer, DmaDirection::DeviceToDriver) }?;
//!
//! // 2. Program the device with dma.bus_addr and wait for completion.
//! // device.submit(dma.bus_addr, buffer.len());
//!
//! // 3. Unmap exactly once with the same address and buffer.
//! unsafe { unmap_dma_buffer(device, dma.bus_addr, buffer, DmaDirection::DeviceToDriver) };
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
mod device;
mod dma;

use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::AllocResult;
pub use device::DmaDeviceContext;
pub use dma::DmaPageTableIf;
use memaddr::PhysAddr;

/// Converts a physical address to a bus address.
///
/// It assumes that there is a linear mapping with the offset
/// [`kbuild_config::PHYS_BUS_OFFSET`]. This legacy helper describes the platform
/// bus relation used by every device context. We have
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
/// This function allocates a block of memory through the global allocator. The
/// memory pages must be contiguous, undivided, and have consistent read and
/// write access.
///
/// - `layout`: The memory layout, which describes the size and alignment
///   requirements of the requested memory.
///
/// Returns an [`DmaInfo`] structure containing details about the allocated
/// memory, including CPU and device addresses. The complete page-rounded extent
/// must fit `device`'s coherent mask. Unreachable allocations are released and
/// return an error; the allocator does not search for low-address alternatives.
///
/// # Errors
///
/// Returns `AllocError::InvalidInput` for a zero-sized layout or an unmet device
/// address/alignment constraint. Returns `AllocError::NoMemory` when pages cannot be allocated, the
/// mapping-attribute update fails, or the platform `prepare` hook fails.
///
/// # Safety
///
/// The caller must free the returned allocation exactly once with
/// [`deallocate_dma_memory`] using the same `device` and `layout`, and keep the
/// context alive until release. Only the represented device may use the returned
/// bus address, and all DMA and CPU accesses must stop before release.
#[track_caller]
pub unsafe fn allocate_dma_memory(
    device: &DmaDeviceContext,
    layout: Layout,
) -> AllocResult<DmaInfo> {
    // SAFETY: forwards the caller's DMA allocation preconditions to the shared DMA backend.
    unsafe { dma::lock_allocator().allocate_dma_memory(device, layout) }
}

/// Frees coherent memory previously allocated.
///
/// This function releases the memory block that was previously allocated and
/// marked as coherent. It ensures proper deallocation and management of resources
/// associated with the memory block.
///
/// `dma` carries the original CPU and bus addresses, and `layout` carries the
/// original requested size and alignment.
///
/// # Safety
///
/// `dma` must come from a prior successful [`allocate_dma_memory`] call, and
/// `device` and `layout` must exactly match that allocation. Release exactly
/// once after stopping all hardware access and retiring CPU aliases.
#[track_caller]
pub unsafe fn deallocate_dma_memory(_device: &DmaDeviceContext, dma: DmaInfo, layout: Layout) {
    // SAFETY: forwards the caller's matching deallocation contract to the shared DMA backend.
    unsafe { dma::lock_allocator().deallocate_dma_memory(dma, layout) }
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
/// memory. Keeping this behind `kdma` lets future implementations restore
/// direct-map optimizations with stricter page-granularity safety checks
/// without changing driver code.
///
/// # Errors
///
/// Returns `AllocError::InvalidInput` for zero length, an unmet device address
/// mask or an already-active address, and
/// `AllocError::NoMemory` when the bounce pool cannot satisfy the
/// request.
///
/// # Safety
///
/// `buffer` must contain initialized bytes and remain live and exclusively owned
/// until [`unmap_dma_buffer`] on the same `device` with the same direction.
/// CPU access while mapped requires an ownership handoff with [`sync_dma_buffer`].
/// Keep the context alive, expose the address only to its represented device,
/// and stop hardware access before unmapping. Receive mappings preserve original
/// bytes for cancellation and partial writes by copying them into the bounce buffer.
pub unsafe fn map_dma_buffer(
    device: &DmaDeviceContext,
    buffer: NonNull<[u8]>,
    direction: DmaDirection,
) -> AllocResult<DmaInfo> {
    // SAFETY: forwards the caller's exclusive-buffer contract to the DMA mapper.
    unsafe { dma::lock_allocator().map_dma_buffer(device, buffer, direction) }
}

/// Unmaps a previously mapped DMA buffer and synchronizes data back if needed.
///
/// # Safety
///
/// `dma_addr` must be the still-active bus address previously returned by
/// [`map_dma_buffer`] on the same `device`, for the same `buffer` and direction.
/// Stop hardware accesses, retire CPU aliases, and unmap exactly once.
/// # Panics
///
/// Panics when `dma_addr` is not an active mapping or when `buffer`'s
/// pointer, length or direction differs from the recorded mapping;
/// both are caller contract violations, not runtime conditions.
///
/// # Example
///
/// The unmap must repeat the exact address, buffer, and length from the
/// matching [`map_dma_buffer`] call — see the crate-level
/// [example](crate#example) for the full map → device → unmap sequence.
pub unsafe fn unmap_dma_buffer(
    _device: &DmaDeviceContext,
    dma_addr: DmaBusAddress,
    buffer: NonNull<[u8]>,
    direction: DmaDirection,
) {
    // SAFETY: forwards the caller's matching mapping contract to the DMA mapper.
    unsafe { dma::lock_allocator().unmap_dma_buffer(dma_addr, buffer, direction) }
}

/// Owner receiving access to an existing streaming mapping.
pub enum DmaSyncTarget {
    /// CPU may access the original buffer after device completion.
    Cpu,
    /// Device may access the DMA buffer after CPU access stops.
    Device,
}

/// Synchronize a mapping for an ownership handoff while retaining it.
///
/// # Errors
/// Returns `AllocError::InvalidInput` when the mapping arguments do not match
/// an active record in the shared backend, before accessing buffer memory.
///
/// # Safety
/// The original buffer must remain initialized, live and exclusively owned.
/// Stop the previous owner's accesses before calling, and restrict subsequent
/// accesses to the selected owner until the next handoff or unmap.
pub unsafe fn sync_dma_buffer(
    _device: &DmaDeviceContext,
    dma_addr: DmaBusAddress,
    buffer: NonNull<[u8]>,
    direction: DmaDirection,
    target: DmaSyncTarget,
) -> AllocResult<()> {
    // SAFETY: the caller establishes the original mapping and ownership handoff contract.
    unsafe { dma::lock_allocator().sync_dma_buffer(dma_addr, buffer, direction, target) }
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

/// Represents information related to a DMA operation.
#[derive(Debug, Clone, Copy)]
pub struct DmaInfo {
    /// The address at which the CPU accesses this memory region. This address
    /// is a virtual memory address used by the CPU to access memory.
    pub cpu_addr: NonNull<u8>,
    /// Represents the physical address of this memory region on the bus. The DMA
    /// controller uses this address to directly access memory.
    pub bus_addr: DmaBusAddress,
}
