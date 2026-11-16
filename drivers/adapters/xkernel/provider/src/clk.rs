// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel adapter between the clock capability contract and `kclk`.
//!
//! The framework's consumer guard is erased here: [`XKernelClockOp`] resolves
//! `(provider id, index)` through the global `kclk` registry and wraps the
//! returned handle so drivers only ever see `driver_os_api::Clk`.

use alloc::sync::Arc;

use driver_os_api::{Clk, ClkConsumer, ClockOp, ResError, ResResult};
use kclk::{ClkError, ClkHandle};

/// Host adapter implementing [`ClockOp`] over the `kclk` registry.
///
/// Stateless; the framework owns the long-lived instance.
#[derive(Debug, Default)]
pub struct XKernelClockOp;

impl XKernelClockOp {
    pub const fn new() -> Self {
        Self
    }
}

/// The framework-owned instance (same shape as the MMIO/IRQ/DMA providers).
pub static XKERNEL_CLOCK_OP: XKernelClockOp = XKernelClockOp;

/// Newtype adapting a `kclk` handle to the capability contract's consumer
/// trait (the orphan-rule bridge).
struct KclkConsumer(ClkHandle);

impl ClkConsumer for KclkConsumer {
    fn prepare_enable(&self) -> ResResult {
        self.0.prepare_enable().map_err(map_clk_error)
    }

    fn disable_unprepare(&self) {
        self.0.disable_unprepare();
    }

    fn get_rate(&self) -> u64 {
        self.0.get_rate()
    }

    fn round_rate(&self, hz: u64) -> ResResult<u64> {
        self.0.round_rate(hz).map_err(map_clk_error)
    }

    fn set_rate(&self, hz: u64) -> ResResult<u64> {
        self.0.set_rate(hz).map_err(map_clk_error)
    }

    fn is_enabled(&self) -> bool {
        self.0.is_enabled().unwrap_or(false)
    }
}

fn map_clk_error(error: ClkError) -> ResError {
    match error {
        ClkError::NoProvider | ClkError::NotFound => ResError::NotFound,
        ClkError::NotSupported => ResError::Unsupported,
        ClkError::InvalidRate | ClkError::InvalidParent | ClkError::Aborted => {
            ResError::InvalidResource
        }
        ClkError::Busy => ResError::Busy,
        ClkError::Io => ResError::Io,
        ClkError::BadState => ResError::BadState,
    }
}

impl ClockOp for XKernelClockOp {
    fn clk_get(&self, provider_id: u32, index: u32) -> ResResult<Clk> {
        let handle = kclk::clk_get_by_index(provider_id, index).map_err(map_clk_error)?;
        Ok(Clk::new(Arc::new(KclkConsumer(handle))))
    }
}
