// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation and mapping tokens for the unified platform DMA domain.

use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::AllocResult;

use crate::{DmaBusAddress, DmaDeviceContext, DmaDirection, DmaInfo, DmaSyncTarget};

static PLATFORM: DmaDeviceContext = DmaDeviceContext::platform_default();

/// Selection policy for one streaming mapping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DmaMappingPolicy {
    /// Use direct DMA when the complete range is eligible, otherwise bounce.
    #[default]
    Auto,
    /// Require direct DMA; return an error when its prerequisites are unmet.
    Direct,
    /// Stage through coherent storage even when direct DMA is eligible.
    Bounce,
}

/// Backend selected for a successful streaming mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaMappingKind {
    /// Hardware accesses the original physical memory.
    Direct,
    /// Hardware accesses separate coherent staging storage.
    Bounce,
}

/// Persistent, zero-initialized coherent memory in the platform DMA domain.
///
/// CPU and device addresses have separate meanings. No streaming sync is needed,
/// but descriptor publication still needs ordering and hardware coordination.
/// The token is neither cloned nor automatically freed: [`Self::free`] requires
/// the caller to establish hardware quiescence. Dropping it retains the pages.
#[must_use = "coherent memory must be explicitly freed after hardware stops"]
#[derive(Debug)]
pub struct CoherentAllocation {
    dma: DmaInfo,
    layout: Layout,
}

impl CoherentAllocation {
    /// Allocate page-backed coherent memory, zeroing the full rounded extent.
    ///
    /// # Errors
    /// Returns the allocation, address/alignment or platform-setup error.
    #[track_caller]
    pub fn allocate(layout: Layout) -> AllocResult<Self> {
        // SAFETY: this token retains the original allocation/layout; free requires
        // quiescence and consumes the token, and no CPU references are exposed.
        let dma = unsafe { crate::allocate_dma_memory(&PLATFORM, layout) }?;
        Ok(Self { dma, layout })
    }

    /// Return the CPU pointer; accesses require caller-managed hardware coordination.
    pub const fn cpu_addr(&self) -> NonNull<u8> {
        self.dma.cpu_addr
    }

    /// Return the address programmed into the device.
    pub const fn bus_addr(&self) -> DmaBusAddress {
        self.dma.bus_addr
    }

    /// Return the original requested size and alignment.
    pub const fn layout(&self) -> Layout {
        self.layout
    }

    /// Consume the allocation and release its original pages and attributes.
    ///
    /// # Safety
    /// Stop all hardware accesses and retire every CPU alias, including any
    /// streaming mappings of this allocation, before release.
    #[track_caller]
    pub unsafe fn free(self) {
        // SAFETY: the token carries the original allocation/layout; caller stopped
        // hardware and retired every CPU alias and dependent streaming mapping.
        unsafe { crate::deallocate_dma_memory(&PLATFORM, self.dma, self.layout) };
    }
}

/// An active mapping retaining its original buffer, direction and backend kind.
///
/// Mapping starts with device ownership. Sync explicitly hands ownership to the
/// CPU or device; unmap returns it to the CPU. Dropping a token retains the
/// backend record and storage, because hardware stop cannot be inferred by Drop.
/// The caller must keep the original buffer live even if the token is dropped.
#[must_use = "streaming mappings must be explicitly unmapped after hardware stops"]
#[derive(Debug)]
pub struct StreamingMapping {
    dma: DmaInfo,
    buffer: NonNull<[u8]>,
    direction: DmaDirection,
    kind: DmaMappingKind,
}

impl StreamingMapping {
    /// Map initialized kernel memory in the unified platform DMA domain.
    ///
    /// # Errors
    /// Returns `InvalidInput` for an empty/overflowing range, conflicting active
    /// DMA addresses, or unmet required-direct prerequisites. Pool exhaustion
    /// and cache-maintenance failures propagate without publishing a mapping.
    ///
    /// # Safety
    /// Retain initialized, pinned kernel RAM with stable mapping attributes until
    /// unmap. The entire buffer and all aliases are exclusively owned. While
    /// device-owned, stop CPU accesses; while CPU-owned, stop device accesses.
    /// Honor the direction: receive buffers are CPU read-only while mapped.
    pub unsafe fn map(
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
        policy: DmaMappingPolicy,
    ) -> AllocResult<Self> {
        let mut allocator = crate::dma::lock_allocator();
        // SAFETY: the caller retains initialized pinned RAM and exclusive access
        // through unmap; this token preserves the complete original arguments.
        let (dma, kind) =
            unsafe { allocator.map_with_policy(&PLATFORM, buffer, direction, policy) }?;
        Ok(Self {
            dma,
            buffer,
            direction,
            kind,
        })
    }

    /// Return the original CPU buffer address, independently of bounce storage.
    pub const fn cpu_addr(&self) -> NonNull<u8> {
        self.buffer.cast()
    }

    /// Return the address programmed into the device.
    pub const fn bus_addr(&self) -> DmaBusAddress {
        self.dma.bus_addr
    }

    /// Return the mapped byte length.
    pub const fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Successful mappings always contain at least one byte.
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Return the selected Direct/Bounce backend.
    pub const fn kind(&self) -> DmaMappingKind {
        self.kind
    }

    /// Hand ownership to the CPU while keeping the mapping active.
    ///
    /// # Errors
    /// Propagates a cache-maintenance failure; ownership stays with the device.
    ///
    /// # Safety
    /// Hardware access has stopped. The original buffer and aliases remain live
    /// and exclusive; CPU writes are permitted only for transmit/bidirectional DMA.
    pub unsafe fn sync_for_cpu(&mut self) -> AllocResult<()> {
        // SAFETY: caller stopped hardware; the token preserves the original mapping.
        unsafe { self.sync(DmaSyncTarget::Cpu) }
    }

    /// Hand ownership to the device after CPU accesses stop.
    ///
    /// # Errors
    /// Propagates a cache-maintenance failure; ownership stays with the CPU.
    ///
    /// # Safety
    /// CPU accesses and aliases have stopped; hardware stays stopped until success.
    pub unsafe fn sync_for_device(&mut self) -> AllocResult<()> {
        // SAFETY: caller stopped CPU access and holds hardware until the transition succeeds.
        unsafe { self.sync(DmaSyncTarget::Device) }
    }

    /// # Safety
    /// The previous owner has stopped all accesses; the original initialized
    /// pinned buffer and its attributes remain live and exclusively owned.
    unsafe fn sync(&self, target: DmaSyncTarget) -> AllocResult<()> {
        // SAFETY: the caller stopped the previous owner; original arguments are retained.
        unsafe {
            crate::sync_dma_buffer(
                &PLATFORM,
                self.dma.bus_addr,
                self.buffer,
                self.direction,
                target,
            )
        }
    }

    /// Return ownership to the CPU and consume the mapping after DMA stops.
    ///
    /// # Errors
    /// A failed handoff returns the error and original token for retry. The
    /// original buffer stays pinned and owned by that mapping until successful unmap.
    ///
    /// # Safety
    /// Hardware and CPU accesses have stopped; the original memory remains live
    /// and exclusively owned until unmap succeeds.
    pub unsafe fn unmap(self) -> Result<(), (alloc_engine::AllocError, Self)> {
        // SAFETY: the caller stopped hardware and CPU accesses; the token supplies
        // its unique original arguments and is consumed exactly once.
        let result = unsafe {
            crate::dma::lock_allocator().try_unmap_dma_buffer(
                self.dma.bus_addr,
                self.buffer,
                self.direction,
            )
        };
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err((error, self)),
        }
    }
}
