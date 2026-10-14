// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver-side runtime resource handles, split by resource family.
//!
//! The guards-in-`Data` contract: each handle owns everything a live
//! resource needs and releases it in the Linux remove order on drop —
//! MMIO mappings stay valid while any guard lives, IRQs detach their
//! handler, clocks release prepare/enable, coherent memory unmaps.

use alloc::sync::Arc;

mod clk;
mod dma;
pub(crate) mod iomap;
mod irq;

pub use clk::{ClkHandle, EnabledClock};
pub use dma::CoherentMem;
pub use iomap::IoMap;
pub use irq::{DevIrq, IrqAction, IrqHandlerFn};

/// Deferred resource release hook carried by every guard.
pub type ReleaseHook = Arc<dyn Fn() + Send + Sync>;
