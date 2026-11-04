// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device DMA capabilities and owned coherent/streaming guards.

use core::{fmt, ptr::NonNull};

use super::ReleaseHook;
use crate::{ResError, ResResult};

mod typed;
pub use typed::{CoherentArray, CoherentBox};

/// Coherent DMA memory; freed on drop.
pub struct CoherentMem {
    owner: CoherentOwner,
}

enum CoherentOwner {
    Provider {
        provider: &'static dyn DmaOp,
        allocation: DmaAllocation,
    },
    Hook {
        cpu: usize,
        bus: u64,
        len: usize,
        release: Option<ReleaseHook>,
    },
}

impl fmt::Debug for CoherentMem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CoherentMem")
            .field("bus", &self.bus_addr())
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl CoherentMem {
    /// Construct a guard for a host allocation with its matching release hook.
    pub fn new(cpu: usize, bus: u64, len: usize, release: Option<ReleaseHook>) -> Self {
        Self {
            owner: CoherentOwner::Hook {
                cpu,
                bus,
                len,
                release,
            },
        }
    }

    /// Acquire coherent storage and retain the exact provider allocation token.
    ///
    /// Device access must stop before drop. This preserves the provider's policy;
    /// it does not infer a device DMA view.
    pub fn alloc_with(provider: &'static dyn DmaOp, spec: DmaSpec) -> ResResult<Self> {
        let allocation = provider.alloc_coherent(spec)?;
        Ok(Self {
            owner: CoherentOwner::Provider {
                provider,
                allocation,
            },
        })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        match &self.owner {
            CoherentOwner::Provider { allocation, .. } => allocation.cpu_addr as *mut u8,
            CoherentOwner::Hook { cpu, .. } => *cpu as *mut u8,
        }
    }

    pub const fn bus_addr(&self) -> u64 {
        match &self.owner {
            CoherentOwner::Provider { allocation, .. } => allocation.bus_addr,
            CoherentOwner::Hook { bus, .. } => *bus,
        }
    }

    pub const fn len(&self) -> usize {
        match &self.owner {
            CoherentOwner::Provider { allocation, .. } => allocation.spec.len,
            CoherentOwner::Hook { len, .. } => *len,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for CoherentMem {
    fn drop(&mut self) {
        match &mut self.owner {
            CoherentOwner::Provider {
                provider,
                allocation,
            } => provider.free_coherent(*allocation),
            CoherentOwner::Hook { release, .. } => {
                if let Some(release) = release.take() {
                    release();
                }
            }
        }
    }
}

/// A held capability for one stable device DMA view.
///
/// Holding the capability does not authorize DMA after device removal. Each
/// returned guard retains its original provider and cleanup dependencies even
/// if the capability or originating service bundle is dropped.
///
/// # Safety
/// Implementations must apply the same stable device window, mask and cache
/// policy to every operation, never select a device from ambient state.
/// Coherent allocations must be valid, exclusive, zeroed, correctly aligned
/// CPU/device-accessible memory for their entire lifetime. Guards must release
/// exactly the original native allocation or mapping once, including required
/// directional synchronization/copy-back. Errors retain no acquired resources;
/// streaming errors must leave the original buffer unchanged. Implementations
/// must never read or copy back uninitialized bytes, including receive bounce
/// storage not written by the device. Mapping followed by release without any
/// device writes must preserve the original bytes; partial device writes must
/// preserve the untouched bytes, including on receive-only mappings.
/// Implementations must document supported
/// acquisition and guard-release execution contexts.
pub unsafe trait DeviceDma: Send + Sync {
    /// Allocate owned coherent memory for this device.
    ///
    /// Stop device access before dropping the returned guard.
    ///
    /// # Errors
    /// Rejects invalid size/alignment, unsupported policy, or unavailable memory.
    fn alloc_coherent(&self, len: usize, align: usize) -> ResResult<CoherentMem>;

    /// Map caller-owned memory until the returned guard is dropped.
    ///
    /// # Safety
    /// The buffer must contain initialized bytes at entry and remain valid,
    /// live and exclusively held for the whole mapping lifetime, including
    /// cleanup. Do not access it through the CPU while
    /// mapped. Device access must stop before guard Drop, which completes any
    /// required copy-back/cache synchronization in the provider's permitted
    /// context. The caller retains ownership of the original buffer.
    ///
    /// # Errors
    /// Mapping failure leaves the buffer unchanged and no live mapping behind.
    unsafe fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<StreamingMapping>;
}

/// One non-cloneable streaming mapping, released exactly once on Drop.
pub struct StreamingMapping {
    bus: u64,
    len: usize,
    release: ReleaseHook,
}

impl fmt::Debug for StreamingMapping {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamingMapping")
            .field("bus", &self.bus)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl StreamingMapping {
    /// Transfer one active mapping into an owned guard.
    ///
    /// # Safety
    /// `bus` and `len` must describe a valid device mapping of the original
    /// exclusive, live buffer. `release` must retain its provider and exact
    /// native token, synchronize and release that mapping once without freeing
    /// the original buffer or copying uninitialized receive bytes. No other
    /// owner may invoke the release operation.
    /// Device access must stop before Drop; the buffer must remain exclusively
    /// held until cleanup completes in the provider's permitted context.
    pub unsafe fn new(bus: u64, len: usize, release: ReleaseHook) -> Self {
        Self { bus, len, release }
    }

    /// Device-visible base address; zero may be a valid bus address.
    pub const fn bus_addr(&self) -> u64 {
        self.bus
    }

    /// Mapped length in bytes.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping has an empty extent.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for StreamingMapping {
    fn drop(&mut self) {
        (self.release)();
    }
}

/// One resolved, contiguous CPU-physical to device-bus DMA window.
///
/// Unlike a local firmware range, both address spaces are fully resolved.
/// Private fields preserve nonempty, nonoverflowing window invariants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaWindow {
    cpu_start: u64,
    bus_start: u64,
    last_offset: u64,
}

impl DmaWindow {
    /// Explicit full-address-space identity mapping, not a missing-data fallback.
    pub const IDENTITY: Self = Self {
        cpu_start: 0,
        bus_start: 0,
        last_offset: u64::MAX,
    };

    /// Whether CPU and device addresses coincide throughout this window.
    pub const fn is_identity(&self) -> bool {
        self.cpu_start == self.bus_start
    }

    /// Construct a resolved window with a length in bytes.
    ///
    /// # Errors
    /// Returns `InvalidResource` for zero length or either address-end overflow.
    pub const fn new(cpu_start: u64, bus_start: u64, len: u64) -> ResResult<Self> {
        let Some(last_offset) = len.checked_sub(1) else {
            return Err(ResError::InvalidResource);
        };
        if cpu_start.checked_add(last_offset).is_none()
            || bus_start.checked_add(last_offset).is_none()
        {
            return Err(ResError::InvalidResource);
        }
        Ok(Self {
            cpu_start,
            bus_start,
            last_offset,
        })
    }
}

/// A device's resolved DMA window, device address mask and cache coherency.
///
/// The device model owns these facts; the host applies them to every allocation
/// and mapping. A bus window is not a device mask. This direct-DMA contract
/// supports one continuous window, not an IOMMU or disjoint range map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaInfo {
    /// Device-usable low address bits (`DMA_BIT_MASK(n)`).
    /// Noncontiguous masks are rejected by translation.
    pub mask: u64,
    /// Resolved CPU-to-device window, independently bounded from the mask.
    pub window: DmaWindow,
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
        window: DmaWindow::IDENTITY,
        coherent: true,
    };

    /// Translate one byte. Allocators must use [`Self::translate_range`] instead.
    pub const fn translate(&self, paddr: u64) -> Option<u64> {
        self.translate_range(paddr, 1)
    }

    /// Translate a complete nonempty physical interval, returning its bus base.
    ///
    /// Rejects lower/upper window violations, zero length, arithmetic overflow,
    /// invalid masks and intervals whose last bus byte exceeds the device mask.
    /// This does not allocate memory or perform cache maintenance.
    pub const fn translate_range(&self, paddr: u64, len: u64) -> Option<u64> {
        if self.mask != u64::MAX && self.mask & (self.mask + 1) != 0 {
            return None;
        }
        let Some(offset) = paddr.checked_sub(self.window.cpu_start) else {
            return None;
        };
        let Some(last_byte) = len.checked_sub(1) else {
            return None;
        };
        let Some(last_offset) = offset.checked_add(last_byte) else {
            return None;
        };
        if last_offset > self.window.last_offset {
            return None;
        }
        if self.window.bus_start + last_offset > self.mask {
            return None;
        }
        Some(self.window.bus_start + offset)
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

/// Raw DMA allocation/mapping contract used by static HAL integrations.
///
/// Raw callers retain the original release tuple, buffer lifetime and cache
/// synchronization obligations. DeviceDma supplies device-scoped policy; this
/// contract does not infer a device window from ambient state.
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
unittest::path_mod_test!(coherent_tests, "../ut-tests/coherent.rs");
