// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO MMIO block device model (ArceOS `axvirtio-blk`).
//!
//! Ported device model on top of [`super::virtio`]: standard MMIO register
//! handling is delegated to [`VirtioMmioState`], this module owns the block
//! configuration space and the request data path (read / write / flush on
//! descriptor chains) plus a pluggable [`BlockBackend`].

mod backend;
mod block;
/// VirtIO block constants (device config offsets, request types, features).
pub mod constants;
mod mmio;

// Re-export from the shared VirtIO module
// Re-export device-specific types
pub use backend::BlockBackend;
pub use block::config::VirtioBlockConfig;
pub use mmio::{BlockDeviceEvent, VirtioMmioBlockDevice};

pub use crate::vdev::virtio::{VirtioConfig, VirtioError, VirtioQueue, VirtioResult};
