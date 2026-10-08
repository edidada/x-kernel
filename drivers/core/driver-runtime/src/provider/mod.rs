// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The X-Kernel resource provider — inlined from the former
//! `device-res-xkernel` adapter crate.
//!
//! `device_res`'s Op traits describe *what* drivers need; this module binds
//! those operations to memspace (MMIO), kirq (interrupts) and the coherent
//! DMA allocator. It lives in driver-runtime now: exactly one consumer
//! (XKernelRuntime + the vendor HAL surface), so an extra crate bought
//! nothing but a dependency edge.

mod dma;
mod irq;
mod mmio;

/// The stateless X-Kernel resource provider.
#[derive(Debug, Default)]
pub struct XKernelResourceProvider;

impl XKernelResourceProvider {
    /// Create a stateless X-Kernel resource provider.
    pub const fn new() -> Self {
        Self
    }
}
