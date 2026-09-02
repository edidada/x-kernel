// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock resource types: the specifier, the consumer-facing capability trait,
//! and the [`Clk`] RAII handle.
//!
//! A device consumes a clock described in firmware (device-tree
//! `clocks = <&ccu 5>`; an optional `clock-names` entry names the consumer's
//! *local* connection, which the platform resolves to that specifier before
//! building a [`ClkSpec`]). The OS-agnostic [`ClkSpec`] carries the resolved
//! `(provider, specifier)` (or a SoC clock name); the host kernel implements
//! [`ClkOp`] to resolve it into a [`ClkConsumer`] trait object wrapping its
//! own clock handle (the x-kernel host adapts this to its clock framework)
//! `kdriver::resource`).

use alloc::sync::Arc;

use crate::{ResError, ResResult};

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
/// may sleep but must not re-enter the framework (it runs under the
/// non-re-entrant global clock lock).
pub type ClkNotifier = Arc<dyn Fn(&ClkRateEvent) -> NotifierVote + Send + Sync>;

/// A specifier describing which clock a device consumes.
///
/// The two forms are mutually exclusive by construction: a by-name lookup
/// carries no (provider, index) pair, and a phandle lookup carries no name.
/// The canonical firmware path is [`ByPhandleIndex`](Self::ByPhandleIndex) —
/// it mirrors the two cells of a DT `clocks = <&phandle index>` specifier,
/// where `provider` is the phandle selecting *which* controller and `index`
/// is the output within that controller. A consumer-local connection name
/// (a DT `clock-names` entry such as `"baudclk"`) is **not** a valid
/// [`ByName`](Self::ByName) argument: connection names are not globally
/// unique and must be resolved by the platform firmware layer into the
/// matching `clocks` specifier first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClkSpec {
    /// Resolve by the clock's **SoC name** — the provider output name
    /// (e.g. `"osc24m"`), globally unique across the SoC.
    ByName(&'static str),
    /// Resolve by `(provider, index)` — the two cells of a DT
    /// `clocks = <&phandle index>` specifier. The canonical DT path.
    ByPhandleIndex {
        /// The provider identifier (the controller's DT phandle).
        provider: u32,
        /// The provider-relative index.
        index: u32,
    },
}

/// One clock connection attached to a device.
///
/// `connection_name` is the consumer-local firmware name (for example
/// `"baudclk"` from `clock-names`); [`spec`](Self::spec) is the normalized
/// provider lookup key. Keeping both values together prevents the name/index
/// association from being lost when firmware data becomes a device resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClkResource {
    connection_name: Option<&'static str>,
    spec: ClkSpec,
}

impl ClkResource {
    /// Construct an unnamed clock connection.
    pub const fn unnamed(spec: ClkSpec) -> Self {
        Self {
            connection_name: None,
            spec,
        }
    }

    /// Construct a consumer-local named clock connection.
    pub const fn named(connection_name: &'static str, spec: ClkSpec) -> Self {
        Self {
            connection_name: Some(connection_name),
            spec,
        }
    }

    /// Consumer-local connection name, if firmware supplied one.
    pub const fn connection_name(self) -> Option<&'static str> {
        self.connection_name
    }

    /// Normalized provider lookup key for this connection.
    pub const fn spec(self) -> ClkSpec {
        self.spec
    }
}

impl ClkSpec {
    /// Resolve by the clock's **SoC name** — the provider output name registered
    /// via the provider's table registration (e.g. `"uart0_clk"`,
    /// `"osc24m"`), globally unique across the SoC.
    ///
    /// This is **not** a DT `clock-names` connection name. A `clock-names`
    /// entry like `"baudclk"` is consumer-local (many devices can have one) and
    /// must be resolved by the platform/firmware layer into the matching
    /// `clocks` specifier, then passed as
    /// [`by_phandle_index`](Self::by_phandle_index).
    pub const fn by_name(name: &'static str) -> Self {
        Self::ByName(name)
    }

    /// Describe a clock by `(provider, index)`, the two cells of a DT
    /// `clocks = <&phandle index>` specifier. This is the canonical DT path:
    /// the platform resolves a consumer's `clock-names` connection name to its
    /// `clocks` specifier and builds this `ClkSpec`.
    pub const fn by_phandle_index(provider: u32, index: u32) -> Self {
        Self::ByPhandleIndex { provider, index }
    }

    /// The SoC clock name, if resolved by name.
    pub const fn name(&self) -> Option<&'static str> {
        match self {
            Self::ByName(name) => Some(*name),
            Self::ByPhandleIndex { .. } => None,
        }
    }

    /// The provider identifier, if resolved by phandle+index.
    pub const fn provider(&self) -> Option<u32> {
        match self {
            Self::ByName(_) => None,
            Self::ByPhandleIndex { provider, .. } => Some(*provider),
        }
    }

    /// The provider-relative index, if resolved by phandle+index.
    pub const fn index(&self) -> Option<u32> {
        match self {
            Self::ByName(_) => None,
            Self::ByPhandleIndex { index, .. } => Some(*index),
        }
    }
}

/// OS-agnostic consumer handle returned by the clock capability provider.
///
/// Host kernels implement this by wrapping their own clock handle so device-res
/// never depends on a concrete framework type. The trait is the full consumer
/// surface — gating, rate, parent, and rate-change notification — so a driver
/// never needs to reach past the resource layer for clock control, matching how
/// MMIO and IRQ expose their full capability here.
///
/// Only [`name`](Self::name) and [`get_rate`](Self::get_rate) are required; the
/// rest have defaults that model a fixed, ungated, rate-transparent clock (the
/// same semantics a real framework returns for a clock that cannot change rate
/// or switch parents — `Err(Unsupported)` at runtime). A host implementation
/// overrides only the methods its hardware actually supports.
///
/// # Execution-context contract
///
/// Runs in normal (non-interrupt) device-probe/removal context. Within one
/// consumer:
/// - [`prepare`](Self::prepare) / [`unprepare`](Self::prepare) /
///   [`set_rate`](Self::set_rate) / [`set_parent`](Self::set_parent) may
///   sleep (PLL lock waits, I2C-controlled sources) and must not be called
///   from interrupt context.
/// - [`enable`](Self::enable) / [`disable`](Self::disable) are the atomic
///   fast path (a gate-bit write) and may be reached from interrupt context;
///   implementations must not sleep.
/// - Notifier callbacks run under the framework's clock lock: they may sleep
///   but must not re-enter any clock operation.
pub trait ClkConsumer: Send + Sync {
    /// Stable identifier of the underlying clock (for logging).
    fn name(&self) -> &str;

    /// Cached rate, in Hz.
    fn get_rate(&self) -> u64;

    /// Prepare the slow path, propagating to the parent first. Default: no-op.
    fn prepare(&self) -> ResResult<()> {
        Ok(())
    }

    /// Release one prepare reference, then propagate to the parent. Default: no-op.
    fn unprepare(&self) {}

    /// Enable the atomic fast path, propagating to the parent first. Default: no-op.
    fn enable(&self) -> ResResult<()> {
        Ok(())
    }

    /// Release one enable reference, then propagate to the parent. Default: no-op.
    fn disable(&self) {}

    /// Prepare then enable, in that order.
    ///
    /// Rolls back `prepare` if `enable` fails, matching Linux `clk_prepare_enable`.
    fn prepare_enable(&self) -> ResResult<()> {
        self.prepare()?;
        if let Err(e) = self.enable() {
            self.unprepare();
            return Err(e);
        }
        Ok(())
    }

    /// Disable then unprepare, in that order.
    fn disable_unprepare(&self) {
        self.disable();
        self.unprepare();
    }

    /// Whether the hardware reports the clock as enabled. Default: `false`.
    fn is_enabled(&self) -> ResResult<bool> {
        Ok(false)
    }

    /// Whether the hardware reports the clock as prepared. Default: `false`.
    fn is_prepared(&self) -> ResResult<bool> {
        Ok(false)
    }

    /// The currently selected parent clock, if any. Default: `None` (a root).
    fn get_parent(&self) -> Option<Arc<dyn ClkConsumer>> {
        None
    }

    /// Closest achievable rate for `rate` without changing state.
    /// Default: the clock cannot change rate, so its current rate is the closest.
    fn round_rate(&self, _rate: u64) -> ResResult<u64> {
        Ok(self.get_rate())
    }

    /// Program a new rate; returns the rate now in effect. Default: unsupported.
    fn set_rate(&self, _rate: u64) -> ResResult<u64> {
        Err(ResError::Unsupported)
    }

    /// Constrain this clock's rate to `[min, max]` for later `set_rate`/
    /// `round_rate` calls. Default: unsupported.
    fn set_rate_range(&self, _min: u64, _max: u64) -> ResResult<()> {
        Err(ResError::Unsupported)
    }

    /// The current `(min, max)` rate range. Default: unbounded.
    fn get_rate_range(&self) -> (u64, u64) {
        (0, u64::MAX)
    }

    /// Select parent `index` and refresh the cached rate. Default: unsupported.
    fn set_parent(&self, _index: u8) -> ResResult<()> {
        Err(ResError::Unsupported)
    }

    /// Register a subscriber invoked around rate changes; returns an opaque
    /// token for [`unregister_notifier`](Self::unregister_notifier).
    /// Default: unsupported.
    fn register_notifier(&self, _cb: ClkNotifier) -> ResResult<u64> {
        Err(ResError::Unsupported)
    }

    /// Remove a subscriber registered via [`register_notifier`](Self::register_notifier).
    ///
    /// Default: `Err(Unsupported)`, symmetric with `register_notifier`'s
    /// default — a host that grants notifier tokens must override **both**
    /// methods as a pair. A default success here would silently leave a
    /// registered callback live after the driver believes it unsubscribed
    /// (a dangling-callback hazard), so the default fails closed instead.
    fn unregister_notifier(&self, _id: u64) -> ResResult<()> {
        Err(ResError::Unsupported)
    }
}

/// Clock capability: resolve a [`ClkSpec`] into a consumer handle.
///
/// A host kernel implements this and passes it explicitly to
/// [`Clk::get_with`] (the framework owns the provider instance, matching the
/// other resource capabilities in this crate's explicit-provider model).
///
/// Runs in normal (non-interrupt) context during device probe. Resolution
/// itself must not sleep indefinitely: a provider that has not registered
/// yet returns `NoProvider` (retryable) rather than blocking, so driver
/// probing can defer instead of stalling.
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
    /// Resolve a clock specifier through an explicit provider.
    pub fn get_with(provider: &'static dyn ClkOp, spec: ClkSpec) -> ResResult<Self> {
        let consumer = provider.get_clock(spec)?;
        Ok(Self { consumer })
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
    /// # Errors
    ///
    /// Forwards [`ClkConsumer::prepare`]; `Unsupported` when the default
    /// consumer models an ungated clock, hardware errors otherwise.
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
    /// # Errors
    ///
    /// Forwards the consumer's prepare+enable pair; rolls back prepare
    /// when enable fails.
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
    ///
    /// # Errors
    ///
    /// Forwards [`ClkConsumer::set_parent`]. The default consumer
    /// (fixed, single-parent clock) returns
    /// [`Unsupported`](ResError::Unsupported); a host that supports parent
    /// switching may additionally return `InvalidResource` for an
    /// out-of-range index or `Busy` when a parent-gated switch is
    /// rejected while prepared.
    pub fn set_parent(&self, index: u8) -> ResResult<()> {
        self.consumer.set_parent(index)
    }

    /// Register a subscriber invoked around rate changes; returns an opaque
    /// token for [`unregister_notifier`](Self::unregister_notifier)].
    ///
    /// # Errors
    ///
    /// Forwards [`ClkConsumer::register_notifier`]. The default consumer
    /// returns [`Unsupported`](ResError::Unsupported) — a host granting
    /// notifier tokens must override **both** registration and
    /// unregistration as a pair.
    pub fn register_notifier(&self, cb: ClkNotifier) -> ResResult<u64> {
        self.consumer.register_notifier(cb)
    }

    /// Remove a subscriber registered via [`register_notifier`](Self::register_notifier)].
    ///
    /// # Errors
    ///
    /// Forwards [`ClkConsumer::unregister_notifier`]. The default consumer
    /// returns [`Unsupported`](ResError::Unsupported) — symmetric with
    /// `register_notifier`'s default so a subscription can never be
    /// silently left live by a half-overridden host.
    pub fn unregister_notifier(&self, id: u64) -> ResResult<()> {
        self.consumer.unregister_notifier(id)
    }
}
