// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The host capability contract (the provider Op traits).
//!
//! Host adapters implement these OS-neutral contracts using kernel facilities.
//! IRQ contracts are re-exported from device_res, whose X-Kernel adapter
//! translates resource descriptions and handlers to kirq. Other resource
//! contracts remain local pending their own migration.
//!
//! Concrete drivers use Device resource services so acquisition participates
//! in device lifetime management. This hidden provider surface is for host
//! integration, not an alternate driver acquisition path.
//! The policy is documented in docs/ai/driver-subsystem-boundaries.md;
//! no dedicated automated driver-boundary check currently enforces it.

use core::ptr::NonNull;

pub use device_res::{
    IrqHandler, IrqHandlerToken, IrqOp, IrqResource, IrqRouteDesc, MsiMessage, MsiResource,
    ResError, ResResult,
};

use crate::error::RuntimeError;
pub use crate::resource::{IrqController, IrqDomainId, IrqTrigger, MmioRegion, Virq};

/// The ONE provider-error → driver-error mapping in the tree.
///
/// `ResError` is the host capability vocabulary and `RuntimeError` is what a
/// driver sees; without a single `From` every host bridge re-derives the
/// mapping (and the classifications drift). Hosts call
/// `RuntimeError::from(err)` / `.map_err(RuntimeError::from)`.
impl From<ResError> for crate::error::RuntimeError {
    fn from(err: ResError) -> Self {
        match err {
            ResError::InvalidResource => Self::Invalid,
            ResError::MappingFailed => Self::Io,
            ResError::NoMemory => Self::NoMemory,
            ResError::Busy => Self::Busy,
            ResError::Unsupported => Self::Unsupported,
        }
    }
}

/// A mapping token returned by [`MmioOp::map_mmio`].
///
/// The provider is responsible for interpreting the token in
/// [`MmioOp::unmap_mmio`]. Drivers never construct this directly; they hold an
/// [`Io`] handle instead.
#[derive(Debug, Clone, Copy)]
pub struct MmioMapping {
    /// Virtual address the CPU uses to access the region.
    pub vaddr: usize,
    /// The physical region this mapping covers.
    pub region: MmioRegion,
}

/// MMIO mapping capability: map a physical device region into CPU-visible
/// virtual memory and release it.
///
/// A host kernel implements this and passes it to its driver framework. Runs
/// in normal (non-interrupt) context during device probe and removal.
pub trait MmioOp: Sync {
    /// Map an MMIO region and return a token for later teardown.
    fn map_mmio(&self, region: MmioRegion, name: &'static str) -> ResResult<MmioMapping>;

    /// Release a mapping previously returned by [`Self::map_mmio`].
    fn unmap_mmio(&self, mapping: MmioMapping);
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

/// Provider-side clock operations behind a [`ClkHandle`].
///
/// The `kclk` subsystem (the common clock framework core, ported from the `clk/tail` branch
/// acceptance) implements this for real hardware clocks; tests implement it
/// with fixed-rate fakes.
pub trait ClkOps: Send + Sync {
    fn name(&self) -> &str;

    fn get_rate(&self) -> u64;

    fn round_rate(&self, rate: u64) -> Result<u64, RuntimeError>;

    fn set_rate(&self, rate: u64) -> Result<u64, RuntimeError>;

    fn prepare(&self) -> Result<(), RuntimeError>;

    fn unprepare(&self);

    fn enable(&self) -> Result<(), RuntimeError>;

    fn disable(&self);

    fn is_prepared(&self) -> bool;

    fn is_enabled(&self) -> bool;
}
