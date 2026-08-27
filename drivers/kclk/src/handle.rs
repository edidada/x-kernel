// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Consumer-side clock handle.

use alloc::sync::Arc;

use crate::{ClkCore, ClkNotifier, ClkNotifierId, ClkResult, Rate};

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

impl core::fmt::Debug for ClkHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Print the clock name only — the underlying node carries locks and
        // register state that are neither Debug-safe nor useful in logs.
        f.debug_struct("ClkHandle")
            .field("name", &self.name())
            .finish()
    }
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

    /// Prepare the slow path, propagating to the parent first.
    pub fn prepare(&self) -> ClkResult {
        self.core.prepare()
    }

    /// Release one prepare reference, then propagate to the parent.
    pub fn unprepare(&self) {
        self.core.unprepare()
    }

    /// Enable the atomic fast path, propagating to the parent first.
    pub fn enable(&self) -> ClkResult {
        self.core.enable()
    }

    /// Release one enable reference, then propagate to the parent.
    pub fn disable(&self) {
        self.core.disable()
    }

    /// Prepare then enable, in that order.
    ///
    /// On enable failure the prepare reference is rolled back, matching Linux
    /// `clk_prepare_enable`.
    pub fn prepare_enable(&self) -> ClkResult {
        self.core.prepare()?;
        if let Err(err) = self.core.enable() {
            self.core.unprepare();
            return Err(err);
        }
        Ok(())
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

    /// Whether the hardware reports the clock as prepared.
    pub fn is_prepared(&self) -> ClkResult<bool> {
        self.core.is_prepared()
    }

    /// The currently selected parent clock, if any.
    pub fn get_parent(&self) -> Option<ClkHandle> {
        self.core.get_parent().map(ClkHandle::new)
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

    /// Constrain this clock's rate to `[min, max]` for later `set_rate`/
    /// `round_rate` calls.
    pub fn set_rate_range(&self, min: Rate, max: Rate) -> ClkResult {
        self.core.set_rate_range(min, max)
    }

    /// The current `(min, max)` rate range.
    pub fn get_rate_range(&self) -> (Rate, Rate) {
        self.core.get_rate_range()
    }

    /// Select parent `index` and refresh the cached rate.
    pub fn set_parent(&self, index: u8) -> ClkResult {
        self.core.set_parent(index)
    }

    /// Register a rate-change subscriber on this clock.
    ///
    /// The subscriber is invoked around rate changes; a `PreRateChange` veto
    /// aborts the change. Returns an id for [`unregister_notifier`](Self::unregister_notifier).
    /// See [`crate::ClkEvent`].
    pub fn register_notifier(&self, cb: ClkNotifier) -> ClkNotifierId {
        self.core.add_notifier(cb)
    }

    /// Remove a subscriber previously registered via [`register_notifier`](Self::register_notifier).
    pub fn unregister_notifier(&self, id: ClkNotifierId) {
        self.core.remove_notifier(id);
    }
}
