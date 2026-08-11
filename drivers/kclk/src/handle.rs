// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Consumer-side clock handle.

use alloc::sync::Arc;

use crate::{ClkCore, ClkNotifier, ClkResult, Rate};

/// A handle to a registered clock.
///
/// Obtained from the provider via [`crate::clk_get_by_name`] or
/// [`crate::clk_get_by_index`]. Mirrors the Linux `struct clk`: a lightweight,
/// cloneable reference whose operations propagate to the underlying
/// [`ClkCore`] and, where relevant, up the parent tree.
#[derive(Clone)]
pub struct ClkHandle {
    core: Arc<ClkCore>,
}

impl ClkHandle {
    /// Wrap an existing clock node. Provider lookups call this internally.
    pub fn new(core: Arc<ClkCore>) -> Self {
        Self { core }
    }

    /// Human-readable name of the underlying clock.
    pub fn name(&self) -> &'static str {
        self.core.name()
    }

    /// Cached rate, in Hz.
    pub fn get_rate(&self) -> Rate {
        self.core.get_rate()
    }

    /// Cached accuracy, in parts per billion.
    pub fn get_accuracy(&self) -> u32 {
        self.core.get_accuracy()
    }

    /// Prepare then enable, in that order.
    pub fn prepare_enable(&self) -> ClkResult {
        self.core.prepare()?;
        self.core.enable()
    }

    /// Disable then unprepare, in that order.
    pub fn disable_unprepare(&self) {
        self.core.disable();
        self.core.unprepare();
    }

    /// Whether the hardware reports the clock as enabled.
    pub fn is_enabled(&self) -> ClkResult<bool> {
        self.core.is_enabled()
    }

    /// Compute the closest achievable rate for `rate` without changing state.
    ///
    /// Delegates to [`ClkCore::round_rate`], which may propagate to the parent.
    pub fn round_rate(&self, rate: Rate) -> ClkResult<Rate> {
        self.core.round_rate(rate)
    }

    /// Program a new rate and return the rate now in effect.
    pub fn set_rate(&self, rate: Rate) -> ClkResult<Rate> {
        self.core.set_rate(rate)
    }

    /// Select parent `index` and refresh the cached rate.
    pub fn set_parent(&self, index: u8) -> ClkResult {
        self.core.set_parent(index)
    }

    /// Register a rate-change subscriber on this clock.
    ///
    /// The subscriber is invoked around rate changes; a `PreRateChange` veto
    /// aborts the change. See [`crate::ClkEvent`].
    pub fn register_notifier(&self, cb: ClkNotifier) {
        self.core.add_notifier(cb);
    }
}
