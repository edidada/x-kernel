// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! driver-core virtio drivers (one module per device family plus the
//! mmio transport). Features select families; see docs/linux-porting-map.md
//! for the per-family Linux→x-kernel mapping.

#![no_std]

#[cfg(feature = "blk")]
pub mod blk;
#[cfg(feature = "gpu")]
pub mod gpu;
#[cfg(feature = "input")]
pub mod input;
#[cfg(feature = "mmio")]
pub mod mmio;
#[cfg(feature = "net")]
pub mod net;
#[cfg(feature = "rng")]
pub mod rng;
#[cfg(feature = "fs9p")]
pub mod virtio_9p;
#[cfg(feature = "vsock")]
pub mod vsock;
