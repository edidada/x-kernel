// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! OS-agnostic device resource model and capability abstraction.
//!
//! This crate describes the hardware resources a driver consumes (MMIO
//! regions, I/O port ranges, interrupts, coherent/streaming DMA buffers, and
//! small execution capabilities such as a monotonic clock)
//! without binding to any particular kernel. A host kernel provides capability
//! implementations — [`MmioOp`], [`DmaOp`], [`IrqOp`], [`TimeOp`] — to its
//! driver framework (the x-kernel one is `driver-runtime`); driver code
//! acquires resources through RAII handles ([`Io`], [`Irq`], [`DmaCoherent`])
//! or, on the driver-core model, the `DeviceCtx` service methods backed by
//! the same providers.
//!
//! Keeping the OS-semantic operations (map/unmap, request/release IRQ,
//! allocate/free DMA) behind per-resource-type traits isolates driver code
//! from the host kernel: porting a driver to another kernel only requires
//! implementing the providers, not rewriting the driver.
#![no_std]

extern crate alloc;

mod dma;
mod irq;
mod mmio;

/// A collection of resources for a single device.
/// Error returned by resource acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResError {
    /// The resource descriptor was invalid (e.g. zero-sized region).
    InvalidResource,
    /// The host failed to establish the requested mapping.
    MappingFailed,
    /// The host ran out of memory while acquiring the resource.
    NoMemory,
    /// The resource is already in use and cannot be shared.
    Busy,
    /// The host does not support this kind of resource.
    Unsupported,
}

/// Result type for resource operations.
pub type ResResult<T = ()> = Result<T, ResError>;

/// Capability traits and RAII handles for host resource providers; the
/// x-kernel implementation lives in `driver-runtime` (the provider bridge)
/// and `device-res-xkernel` (the host operations).
pub use dma::*;
pub use irq::*;
pub use mmio::*;
