// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel capability implementations for [`driver_os_api`].
//!
//! Binds MMIO, IRQ, CPU memory, raw DMA and monotonic time to the host kernel.
//! The static-provider IRQ entry shares the registration, synchronous
//! release and MSI backend with [`NativeIrq`]. Device identity, service
//! composition and managed cleanup remain in the driver framework.

#![no_std]

extern crate alloc;

mod dma;
mod irq;
mod memory;
mod mmio;
mod msi;
mod native_irq;
mod time;

pub use native_irq::NativeIrq;

/// X-Kernel implementation of the OS-agnostic resource provider traits.
///
/// The type is intentionally stateless. The driver framework owns the
/// long-lived instance and passes its os-api capabilities to consumers.
#[derive(Debug, Default)]
pub struct XKernelResourceProvider;

impl XKernelResourceProvider {
    /// Create a stateless X-Kernel resource provider.
    pub const fn new() -> Self {
        Self
    }
}
