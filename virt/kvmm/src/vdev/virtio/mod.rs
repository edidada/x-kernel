// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ported VirtIO MMIO infrastructure (ArceOS `axvirtio-common`).
//!
//! Shared VirtIO device-model infrastructure: split virtqueue rings, the
//! standard VirtIO MMIO transport register set, guest-memory access
//! abstractions, constants and error types. The block device model built on
//! top of this module lives in [`super::virtio_blk`].

pub mod config;
pub mod constants;
mod device_type;
pub mod error;
pub mod memory;
pub mod mmio;
pub mod queue;

mod addr;

/// Re-export commonly used types
pub use addr::{AccessWidth, GuestPhysAddr, PhysAddr};
pub use config::VirtioConfig;
pub use constants::*;
pub use device_type::VirtioDeviceID;
pub use error::{VirtioError, VirtioResult};
pub use memory::{AddressSpaceMemory, GuestMemory, GuestMemoryAccessor, NoGuestMemoryAccessor};
pub use mmio::state::{MmioReadOutcome, MmioWriteAction, VirtioMmioState};
pub use queue::{DescriptorChain, VirtioQueue};
