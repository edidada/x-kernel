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

/// A specifier describing which clock a device consumes.
///
/// Resolution prefers [`name`](Self::name) (a DT `clock-names` entry) and falls
/// back to [`index`](Self::index) (the cell position in `clocks`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClkSpec {
    name: Option<&'static str>,
    index: u32,
}

impl ClkSpec {
    /// Describe a clock by its consumer connection name.
    pub const fn by_name(name: &'static str) -> Self {
        Self {
            name: Some(name),
            index: 0,
        }
    }

    /// Describe a clock by its position in the provider's `clocks` cell array.
    pub const fn by_index(index: u32) -> Self {
        Self { name: None, index }
    }

    /// The consumer connection name, if described by firmware.
    pub const fn name(&self) -> Option<&'static str> {
        self.name
    }

    /// The provider-relative index.
    pub const fn index(&self) -> u32 {
        self.index
    }
}

/// OS-agnostic consumer handle returned by the clock capability provider.
///
/// Host kernels implement this by wrapping their own clock handle so device-res
/// never depends on a concrete framework type. Operations mirror the Linux
/// `struct clk` consumer API: rates are read-only here; mutable rate and parent
/// operations belong to the clock framework core, not the resource layer.
pub trait ClkConsumer: Send + Sync {
    /// Stable identifier of the underlying clock (for logging).
    fn name(&self) -> &str;

    /// Cached rate, in Hz.
    fn get_rate(&self) -> u64;

    /// Prepare then enable, in that order.
    fn prepare_enable(&self) -> ResResult<()>;

    /// Disable then unprepare, in that order.
    fn disable_unprepare(&self);

    /// Whether the hardware reports the clock as enabled.
    fn is_enabled(&self) -> ResResult<bool>;
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
}
