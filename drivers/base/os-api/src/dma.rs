// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The DMA family: the coherent-memory guard.

use core::{fmt, ptr::NonNull};

use super::ReleaseHook;
use crate::{ResError, ResResult};

/// Coherent DMA memory; freed on drop.
pub struct CoherentMem {
    cpu: usize,
    bus: u64,
    len: usize,
    release: Option<ReleaseHook>,
}

impl fmt::Debug for CoherentMem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoherentMem")
            .field("bus", &self.bus)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl CoherentMem {
    /// Construct a guard for a host allocation with its matching release hook.
    pub fn new(cpu: usize, bus: u64, len: usize, release: Option<ReleaseHook>) -> Self {
        Self {
            cpu,
            bus,
            len,
            release,
        }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.cpu as *mut u8
    }

    pub const fn bus_addr(&self) -> u64 {
        self.bus
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for CoherentMem {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// A device's DMA view of physical memory — `struct device`'s
/// `dma_mask`/`dma_pfn_offset`/coherency triad, snapshotted at enumeration
/// from firmware facts.
///
/// * `mask` (`dev->dma_mask`): the address bits the device actually drives.
///   Every allocation handed to the device must land under it.
/// * `offset` (`dev->dma_pfn_offset << PAGE_SHIFT` with sign): the
///   cumulative `dma-ranges` translation, `dma_addr = paddr - offset`.
///   Signed: a bus can map device ids ABOVE the CPU window.
/// * `coherent` (`dev_is_dma_coherent`): whether the device sees CPU caches;
///   when `false` the host must run the DmaOp cache-maintenance protocol
///   around buffers.
///
/// Firmware facts supplied by the device model and applied by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaInfo {
    /// Device-usable address bits (`DMA_BIT_MASK(n)`); translated device
    /// addresses must satisfy `addr & !mask == 0`.
    pub mask: u64,
    /// `dma_addr = paddr - offset` (signed, cumulative `dma-ranges`).
    pub offset: i64,
    /// Firmware-stated cache coherency (`dma-coherent`).
    pub coherent: bool,
}

impl DmaInfo {
    /// The host default for devices with NO firmware DMA description:
    /// full 64-bit identity view, coherent. Only board-static devices and
    /// firmware-less hosts fall back to this; firmware-described devices
    /// carry firmware's statement instead.
    pub const DEFAULT: Self = Self {
        mask: u64::MAX,
        offset: 0,
        coherent: true,
    };

    /// Translate a CPU physical address into this device's DMA address,
    /// rejecting addresses the device cannot address.
    ///
    /// `None` means the allocation cannot work for this device (outside the
    /// `dma-ranges` window or above `mask`) — callers fail the request
    /// instead of silently wrapping a 64-bit bus address into 32 truncated
    /// device bits.
    pub const fn translate(&self, paddr: u64) -> Option<u64> {
        let dma = paddr as i128 - self.offset as i128;
        if dma < 0 {
            return None;
        }
        let dma = dma as u64;
        if dma & !self.mask != 0 {
            return None;
        }
        Some(dma)
    }
}

/// A request for a coherent DMA buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaSpec {
    /// Buffer length in bytes.
    pub len: usize,
    /// Required alignment in bytes (power of two).
    pub align: usize,
    /// Transfer direction, used by streaming mappings and cache sync.
    pub direction: DmaDirection,
}

impl DmaSpec {
    /// Construct a spec with [`DmaDirection::Bidirectional`] (legacy default).
    pub const fn new(len: usize, align: usize) -> Self {
        Self {
            len,
            align,
            direction: DmaDirection::Bidirectional,
        }
    }

    /// Builder: override the transfer direction.
    pub const fn with_direction(mut self, d: DmaDirection) -> Self {
        self.direction = d;
        self
    }
}

/// A coherent DMA allocation returned by [`DmaOp::alloc_coherent`].
#[derive(Debug, Clone, Copy)]
pub struct DmaAllocation {
    /// Virtual address the CPU uses to access the buffer.
    pub cpu_addr: usize,
    /// Bus address the device uses to access the buffer.
    pub bus_addr: u64,
    /// The originating allocation request.
    pub spec: DmaSpec,
}

/// A streaming DMA mapping returned by [`DmaOp::map_streaming`].
#[derive(Debug, Clone, Copy)]
pub struct DmaMapping {
    /// CPU virtual address of the mapped buffer.
    pub cpu_addr: usize,
    /// Device bus address the device uses to access the buffer.
    pub bus_addr: u64,
    /// Mapped length in bytes.
    pub len: usize,
    /// Transfer direction the mapping was established with.
    pub direction: DmaDirection,
}

pub trait DmaOp: Sync {
    /// Allocate a coherent DMA buffer.
    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation>;

    /// Free a coherent DMA buffer previously returned by [`Self::alloc_coherent`].
    fn free_coherent(&self, alloc: DmaAllocation);

    /// Map an existing caller-owned buffer for streaming DMA.
    ///
    /// Unlike [`Self::alloc_coherent`], the buffer is not owned by the DMA
    /// layer; the mapping is temporary and reversed by [`Self::unmap_streaming`].
    /// Hosts without explicit streaming support may implement this via bounce
    /// buffering. Default: [`Unsupported`](ResError::Unsupported).
    fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<DmaMapping> {
        let _ = (buffer, direction);
        Err(ResError::Unsupported)
    }

    /// Release a streaming mapping previously established by
    /// [`Self::map_streaming`]. Default: no-op.
    fn unmap_streaming(&self, mapping: DmaMapping) {
        let _ = mapping;
    }

    /// Synchronise a streaming mapping before the device reads it (CPU →
    /// device). Default: no-op (coherent / bounce hosts need no maintenance).
    fn sync_for_device(&self, mapping: DmaMapping) {
        let _ = mapping;
    }

    /// Synchronise a streaming mapping before the CPU reads it (device → CPU).
    /// Default: no-op (coherent / bounce hosts need no maintenance).
    fn sync_for_cpu(&self, mapping: DmaMapping) {
        let _ = mapping;
    }

    /// Explicitly flush a CPU address range for non-coherent DMA.
    ///
    /// Default: no-op (bounce-buffer or coherent hosts need no maintenance).
    fn flush_cache(&self, addr: NonNull<u8>, len: usize) {
        let _ = (addr, len);
    }

    /// Explicitly invalidate a CPU address range for non-coherent DMA.
    ///
    /// Default: no-op (bounce-buffer or coherent hosts need no maintenance).
    fn invalidate_cache(&self, addr: NonNull<u8>, len: usize) {
        let _ = (addr, len);
    }
}

/// Direction of a DMA transfer.
///
/// OS-neutral. Host kernels translate this into their own direction
/// representation when performing cache maintenance or bounce buffering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaDirection {
    /// CPU writes, device reads (host flushes before device access).
    DriverToDevice,
    /// Device writes, CPU reads (host invalidates before CPU access).
    DeviceToDriver,
    /// Bidirectional (host flushes and invalidates as needed).
    Bidirectional,
}

unittest::path_mod_test!(tests, "../ut-tests/dma.rs");
