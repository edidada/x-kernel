// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! OS capability contracts required by the driver framework.
//!
//! Hosts implement operations; callers retain capability guards and arrange
//! their release order. This crate owns no device identity, probe state,
//! provider registry or kernel implementation. Legacy device-res remains
//! separate during migration.
#![no_std]

extern crate alloc;

mod clock;
mod dma;
mod error;
mod irq;
mod memory;
mod mmio;
mod time;

use alloc::sync::Arc;

/// Deferred host release operation retained by a capability guard.
///
/// The host must supply a hook that releases this acquisition exactly once.
/// Callers must quiesce hardware and dependent operations before guard drop.
pub type ReleaseHook = Arc<dyn Fn() + Send + Sync>;

pub use clock::{ClkHandle, ClkOps, ClkSpec, EnabledClock};
pub use dma::{
    CoherentArray, CoherentBox, CoherentMem, DeviceDma, DmaAllocation, DmaDirection, DmaInfo,
    DmaMapping, DmaOp, DmaSpec, DmaWindow, StreamingMapping,
};
pub use error::{ResError, ResResult};
pub use irq::{
    DevIrq, IrqController, IrqDomainId, IrqEvent, IrqHandler, IrqHandlerToken, IrqOp, IrqResource,
    IrqRouteDesc, IrqSpec, IrqThreadHandler, IrqTrigger, MsiMessage, MsiResource, Virq,
};
pub use memory::{CpuBuffer, CpuMemoryOp};
pub use mmio::{IoMap, IoPortRange, MmioMapping, MmioOp, MmioRegion};
pub use time::TimeOp;

unittest::path_mod_test!(tests, "../ut-tests/guards.rs");
