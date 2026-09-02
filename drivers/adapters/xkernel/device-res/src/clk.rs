// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel clock provider adapter for [`device_res::ClkOp`].
//!
//! Bridges the OS-agnostic clock capability contract to the `kclk` framework
//! engine (in `adapters/xkernel/kclk/`), the same way `irq.rs` bridges to
//! `kirq`. The kclk core does not depend on devres.

use alloc::sync::Arc;

use device_res::{
    ClkConsumer, ClkNotifier, ClkOp, ClkRateEvent, ClkSpec, NotifierVote, ResError, ResResult,
};
use kclk::ClkHandle;

/// The [`ClkOp`] implementation: resolves a [`ClkSpec`] through the kclk
/// provider registry into a consumer handle.
#[derive(Debug, Default)]
pub struct XKernelClkOp;

impl ClkOp for XKernelClkOp {
    fn get_clock(&self, spec: ClkSpec) -> ResResult<Arc<dyn ClkConsumer>> {
        let handle = match spec.name() {
            Some(name) => kclk::clk_get_by_name(name).map_err(map_clk_err)?,
            None => kclk::clk_get_by_index(spec.provider(), spec.index()).map_err(map_clk_err)?,
        };
        Ok(Arc::new(KclkConsumer(handle)))
    }
}

/// Wraps a `kclk` consumer handle as a device-res [`ClkConsumer`].
struct KclkConsumer(ClkHandle);

impl ClkConsumer for KclkConsumer {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn get_rate(&self) -> u64 {
        self.0.get_rate()
    }

    fn prepare(&self) -> ResResult<()> {
        self.0.prepare().map_err(map_clk_err)
    }

    fn unprepare(&self) {
        let _ = self.0.unprepare();
    }

    fn enable(&self) -> ResResult<()> {
        self.0.enable().map_err(map_clk_err)
    }

    fn disable(&self) {
        self.0.disable()
    }

    fn prepare_enable(&self) -> ResResult<()> {
        self.0.prepare_enable().map_err(map_clk_err)
    }

    fn disable_unprepare(&self) {
        self.0.disable_unprepare()
    }

    fn is_enabled(&self) -> ResResult<bool> {
        self.0.is_enabled().map_err(map_clk_err)
    }

    fn is_prepared(&self) -> ResResult<bool> {
        self.0.is_prepared().map_err(map_clk_err)
    }

    fn get_parent(&self) -> Option<Arc<dyn ClkConsumer>> {
        self.0
            .get_parent()
            .map(|p| Arc::new(KclkConsumer(p)) as Arc<dyn ClkConsumer>)
    }

    fn round_rate(&self, rate: u64) -> ResResult<u64> {
        self.0.round_rate(rate).map_err(map_clk_err)
    }

    fn set_rate(&self, rate: u64) -> ResResult<u64> {
        self.0.set_rate(rate).map_err(map_clk_err)
    }

    fn set_rate_range(&self, min: u64, max: u64) -> ResResult<()> {
        self.0.set_rate_range(min, max).map_err(map_clk_err)
    }

    fn get_rate_range(&self) -> (u64, u64) {
        self.0.get_rate_range()
    }

    fn set_parent(&self, index: u8) -> ResResult<()> {
        self.0.set_parent(index).map_err(map_clk_err)
    }

    fn register_notifier(&self, cb: ClkNotifier) -> ResResult<u64> {
        // Bridge the framework-agnostic callback to kclk's notifier, translating
        // the event types both ways.
        let kclk_cb: kclk::ClkNotifier = Arc::new(move |event| {
            let mapped = match *event {
                kclk::ClkEvent::PreRateChange { old, new } => {
                    ClkRateEvent::PreRateChange { old, new }
                }
                kclk::ClkEvent::PostRateChange { old, new } => {
                    ClkRateEvent::PostRateChange { old, new }
                }
                kclk::ClkEvent::RateChangeAborted { old, attempted } => {
                    ClkRateEvent::RateChangeAborted { old, attempted }
                }
            };
            match cb(&mapped) {
                NotifierVote::Proceed => kclk::NotifierResult::Proceed,
                NotifierVote::Stop => kclk::NotifierResult::Stop,
            }
        });
        Ok(self.0.register_notifier(kclk_cb))
    }

    fn unregister_notifier(&self, id: u64) -> ResResult<()> {
        self.0.unregister_notifier(id);
        Ok(())
    }
}

/// Translate a kclk error into the device-res resource error vocabulary.
fn map_clk_err(err: kclk::ClkError) -> ResError {
    match err {
        kclk::ClkError::NoProvider => ResError::NoProvider,
        kclk::ClkError::NotFound => ResError::NotFound,
        kclk::ClkError::NotSupported => ResError::Unsupported,
        kclk::ClkError::InvalidRate | kclk::ClkError::InvalidParent => ResError::InvalidResource,
        kclk::ClkError::Busy | kclk::ClkError::Aborted => ResError::Busy,
        kclk::ClkError::Io | kclk::ClkError::ParentError | kclk::ClkError::BadState => {
            ResError::InvalidResource
        }
    }
}

#[cfg(unittest)]
mod tests {
    use device_res::{ClkOp, ClkSpec, ResError};
    use unittest::def_test;

    use super::XKernelClkOp;

    #[def_test(serial)]
    fn absent_clock_provider_remains_retryable() {
        kclk::reset_providers();
        assert!(matches!(
            XKernelClkOp.get_clock(ClkSpec::by_phandle_index(0xfeed, 0)),
            Err(ResError::NoProvider)
        ));
    }
}
