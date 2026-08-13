// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock resource types: the specifier, the consumer-facing capability trait,
//! and the [`Clk`] RAII handle.
//!
//! A device consumes a clock described in firmware (device-tree
//! `clocks = <&ccu 5>` plus an optional `clock-names` entry). The
//! OS-agnostic [`ClkSpec`] carries that description; the host kernel
//! implements [`ClkOp`] to resolve it into a [`ClkConsumer`] trait object
//! wrapping its own clock handle (x-kernel adapts to `kclk::ClkHandle` in
//! `kdriver::resource`).

use alloc::sync::Arc;

use crate::{
    ResResult,
    provider::{clk_provider, try_clk_provider},
};

/// A rate-change event delivered to a [`ClkNotifier`] subscriber.
///
/// This is device-res's own view of the event: it carries only the rates, not
/// any framework-specific type, so the resource layer never imports a concrete
/// clock framework. The host backend translates to/from its native event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClkRateEvent {
    /// The rate is about to change from `old` to `new`; a subscriber may veto.
    PreRateChange {
        /// Current rate, before the change.
        old: u64,
        /// Proposed new rate.
        new: u64,
    },
    /// The rate has changed from `old` to `new`.
    PostRateChange {
        /// Previous rate.
        old: u64,
        /// New rate now in effect.
        new: u64,
    },
    /// A rate change was vetoed or failed; the rate remains `old`.
    RateChangeAborted {
        /// Rate that remains in effect.
        old: u64,
        /// Rate that was attempted.
        attempted: u64,
    },
}

/// Whether a [`ClkRateEvent::PreRateChange`] may proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifierVote {
    /// Allow the change to proceed.
    #[default]
    Proceed,
    /// Veto an upcoming rate change.
    Stop,
}

/// A clock rate-change subscriber callback, framework-agnostic.
///
/// The host backend wraps this in whatever concrete notifier type its clock
/// framework expects. Implementations run under the framework's clock lock and
/// must be quick, non-sleeping, and non-re-entrant.
pub type ClkNotifier = Arc<dyn Fn(&ClkRateEvent) -> NotifierVote + Send + Sync>;

/// A specifier describing which clock a device consumes.
///
/// Mirrors the two cells of a DT `clocks = <&phandle index>` specifier: the
/// `provider` is the phandle selecting *which* controller, and `index` is the
/// output within that controller. For consumer convenience, a globally-unique
/// `name` (a `clock-names` entry) may be used instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClkSpec {
    name: Option<&'static str>,
    provider: u32,
    index: u32,
}

impl ClkSpec {
    /// Describe a clock by its consumer connection name (globally unique across
    /// all providers).
    pub const fn by_name(name: &'static str) -> Self {
        Self {
            name: Some(name),
            provider: 0,
            index: 0,
        }
    }

    /// Describe a clock by `(provider, index)`, the two cells of a DT
    /// `clocks = <&phandle index>` specifier.
    pub const fn by_phandle_index(provider: u32, index: u32) -> Self {
        Self {
            name: None,
            provider,
            index,
        }
    }

    /// The consumer connection name, if resolved by name.
    pub const fn name(&self) -> Option<&'static str> {
        self.name
    }

    /// The provider identifier (the controller's DT phandle).
    pub const fn provider(&self) -> u32 {
        self.provider
    }

    /// The provider-relative index.
    pub const fn index(&self) -> u32 {
        self.index
    }
}

/// OS-agnostic consumer handle returned by the clock capability provider.
///
/// Host kernels implement this by wrapping their own clock handle so device-res
/// never depends on a concrete framework type. The trait is the full consumer
/// surface — gating, rate, parent, and rate-change notification — so a driver
/// never needs to reach past the resource layer for clock control, matching how
/// MMIO and IRQ expose their full capability here.
pub trait ClkConsumer: Send + Sync {
    /// Stable identifier of the underlying clock (for logging).
    fn name(&self) -> &str;

    /// Cached rate, in Hz.
    fn get_rate(&self) -> u64;

    /// Prepare the slow path, propagating to the parent first.
    fn prepare(&self) -> ResResult<()>;

    /// Release one prepare reference, then propagate to the parent.
    fn unprepare(&self);

    /// Enable the atomic fast path, propagating to the parent first.
    fn enable(&self) -> ResResult<()>;

    /// Release one enable reference, then propagate to the parent.
    fn disable(&self);

    /// Prepare then enable, in that order.
    fn prepare_enable(&self) -> ResResult<()>;

    /// Disable then unprepare, in that order.
    fn disable_unprepare(&self);

    /// Whether the hardware reports the clock as enabled.
    fn is_enabled(&self) -> ResResult<bool>;

    /// Whether the hardware reports the clock as prepared.
    fn is_prepared(&self) -> ResResult<bool>;

    /// The currently selected parent clock, if any.
    fn get_parent(&self) -> Option<Arc<dyn ClkConsumer>>;

    /// Closest achievable rate for `rate` without changing state.
    fn round_rate(&self, rate: u64) -> ResResult<u64>;

    /// Program a new rate; returns the rate now in effect.
    fn set_rate(&self, rate: u64) -> ResResult<u64>;

    /// Constrain this clock's rate to `[min, max]` for later `set_rate`/
    /// `round_rate` calls.
    fn set_rate_range(&self, min: u64, max: u64) -> ResResult<()>;

    /// The current `(min, max)` rate range.
    fn get_rate_range(&self) -> (u64, u64);

    /// Select parent `index` and refresh the cached rate.
    fn set_parent(&self, index: u8) -> ResResult<()>;

    /// Register a subscriber invoked around rate changes; returns an opaque
    /// token for [`unregister_notifier`](Self::unregister_notifier).
    fn register_notifier(&self, cb: ClkNotifier) -> ResResult<u64>;

    /// Remove a subscriber registered via [`register_notifier`](Self::register_notifier).
    fn unregister_notifier(&self, id: u64) -> ResResult<()>;
}

/// Clock capability: resolve a [`ClkSpec`] into a consumer handle.
///
/// A host kernel implements this and installs it once via
/// [`crate::set_clk_provider`].
pub trait ClkOp: Sync {
    /// Resolve `spec` into a clock consumer handle.
    fn get_clock(&self, spec: ClkSpec) -> ResResult<Arc<dyn ClkConsumer>>;
}

/// RAII handle to a clock consumer.
///
/// Cheap to clone (it shares the underlying consumer via [`Arc`]); the last
/// clone released drops the reference, so callers do not need an explicit
/// `put` call.
#[derive(Clone)]
pub struct Clk {
    consumer: Arc<dyn ClkConsumer>,
}

impl Clk {
    /// Resolve a clock specifier through the installed provider.
    pub fn get(spec: ClkSpec) -> ResResult<Self> {
        let consumer = clk_provider()?.get_clock(spec)?;
        Ok(Self { consumer })
    }

    /// Try to resolve a clock specifier without installing a provider.
    ///
    /// Returns [`None`] before [`crate::set_clk_provider`] has run, instead of
    /// the [`NoProvider`](crate::ResError::NoProvider) error.
    pub fn try_get(spec: ClkSpec) -> ResResult<Option<Self>> {
        match try_clk_provider() {
            Some(provider) => Ok(Some(Self {
                consumer: provider.get_clock(spec)?,
            })),
            None => Ok(None),
        }
    }

    /// Stable identifier of the underlying clock.
    pub fn name(&self) -> &str {
        self.consumer.name()
    }

    /// Cached rate, in Hz.
    pub fn get_rate(&self) -> u64 {
        self.consumer.get_rate()
    }

    /// Prepare the slow path, propagating to the parent first.
    pub fn prepare(&self) -> ResResult<()> {
        self.consumer.prepare()
    }

    /// Release one prepare reference, then propagate to the parent.
    pub fn unprepare(&self) {
        self.consumer.unprepare()
    }

    /// Enable the atomic fast path, propagating to the parent first.
    pub fn enable(&self) -> ResResult<()> {
        self.consumer.enable()
    }

    /// Release one enable reference, then propagate to the parent.
    pub fn disable(&self) {
        self.consumer.disable()
    }

    /// Prepare then enable the clock.
    pub fn prepare_enable(&self) -> ResResult<()> {
        self.consumer.prepare_enable()
    }

    /// Disable then unprepare the clock.
    pub fn disable_unprepare(&self) {
        self.consumer.disable_unprepare()
    }

    /// Whether the hardware reports the clock as enabled.
    pub fn is_enabled(&self) -> ResResult<bool> {
        self.consumer.is_enabled()
    }

    /// Whether the hardware reports the clock as prepared.
    pub fn is_prepared(&self) -> ResResult<bool> {
        self.consumer.is_prepared()
    }

    /// The currently selected parent clock, if any.
    pub fn get_parent(&self) -> Option<Clk> {
        self.consumer.get_parent().map(|consumer| Clk { consumer })
    }

    /// Closest achievable rate for `rate` without changing state.
    pub fn round_rate(&self, rate: u64) -> ResResult<u64> {
        self.consumer.round_rate(rate)
    }

    /// Program a new rate; returns the rate now in effect.
    pub fn set_rate(&self, rate: u64) -> ResResult<u64> {
        self.consumer.set_rate(rate)
    }

    /// Constrain this clock's rate to `[min, max]` for later `set_rate`/
    /// `round_rate` calls.
    pub fn set_rate_range(&self, min: u64, max: u64) -> ResResult<()> {
        self.consumer.set_rate_range(min, max)
    }

    /// The current `(min, max)` rate range.
    pub fn get_rate_range(&self) -> (u64, u64) {
        self.consumer.get_rate_range()
    }

    /// Select parent `index` and refresh the cached rate.
    pub fn set_parent(&self, index: u8) -> ResResult<()> {
        self.consumer.set_parent(index)
    }

    /// Register a subscriber invoked around rate changes; returns an opaque
    /// token for [`unregister_notifier`](Self::unregister_notifier).
    pub fn register_notifier(&self, cb: ClkNotifier) -> ResResult<u64> {
        self.consumer.register_notifier(cb)
    }

    /// Remove a subscriber registered via [`register_notifier`](Self::register_notifier).
    pub fn unregister_notifier(&self, id: u64) -> ResResult<()> {
        self.consumer.unregister_notifier(id)
    }
}
