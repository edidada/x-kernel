// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device resources, one axis, two faces (the Linux `devres` split).
//!
//! - **The front** ([`iomap`], [`irq`], [`dma`], [`clk`]): what a bound driver
//!   acquires through `Device` — ownership-typed guards (`IoMap`, `DevIrq`,
//!   `CoherentMem`, `ClkHandle`). The mapping lives for as long as the guard;
//!   dropping it is release.
//! - **The back** ([`host_api`]): the capability traits (`IrqOp`, `MmioOp`,
//!   `DmaOp`) the host bridge implements over kernel facilities (kirq,
//!   memspace, kdma). The domain runs on mocks by implementing the back.
//!   `#[doc(hidden)]`: host-facing, never driver-facing.
//!
//! [`services`] carries the seam between the two: the per-device service
//! surface (`DeviceServices`) that fronts the back without naming it.
use alloc::sync::Arc;

pub mod clk;
pub mod dma;
#[doc(hidden)]
pub mod host_api;
pub mod iomap;
pub mod irq;
pub mod phy;
pub mod reset;
pub mod services;

/// Deferred resource release hook carried by every guard.
pub type ReleaseHook = Arc<dyn Fn() + Send + Sync>;

pub use clk::{ClkHandle, EnabledClock};
pub use dma::CoherentMem;
pub use iomap::IoMap;
pub use irq::{DevIrq, IrqAction, IrqHandlerFn};
pub use phy::{PhyHandle, PoweredPhy};
pub use reset::{DeassertedGuard, ResetHandle};
pub use services::{DeviceResourceServices, ServiceError};
