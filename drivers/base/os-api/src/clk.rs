// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock capability contract.
//!
//! A driver resolves its device-tree clock connections to a
//! (provider, index) pair through the firmware layer, then acquires a
//! [`Clk`] reference from the host's [`ClockOp`]. The guard carries the
//! clock-framework consumer object behind an erased trait, so drivers never
//! name the concrete clock framework.
//!
//! Acquisition does not change hardware state: the driver explicitly
//! `prepare_enable`s the clocks it needs and balances them with
//! `disable_unprepare` (or relies on the devm wrapper's retirement cleanup).

use alloc::sync::Arc;

use crate::ResResult;

/// Host adapter over one clock reference.
///
/// Implemented by the concrete clock framework; drivers only ever hold the
/// [`Clk`] guard.
pub trait ClkConsumer: Send + Sync {
    /// `clk_prepare` + `clk_enable`, propagating to the parent chain.
    ///
    /// # Errors
    ///
    /// Returns the framework's failure (already-propagated state is rolled
    /// back before returning).
    fn prepare_enable(&self) -> ResResult;

    /// `clk_disable` + `clk_unprepare`, walking the parent chain in reverse.
    fn disable_unprepare(&self);

    /// Current cached rate in Hz.
    fn get_rate(&self) -> u64;

    /// Rounds a rate request without programming hardware.
    ///
    /// # Errors
    ///
    /// Returns the framework's failure when the request cannot be satisfied.
    fn round_rate(&self, hz: u64) -> ResResult<u64>;

    /// Programs a new rate.
    ///
    /// # Errors
    ///
    /// Returns the framework's failure when the request cannot be satisfied.
    fn set_rate(&self, hz: u64) -> ResResult<u64>;

    /// Whether the clock is currently enabled.
    fn is_enabled(&self) -> bool;
}

/// One clock reference held by a driver.
///
/// Cloneable: each clone is a separate consumer (matching `clk_get`
/// returning a fresh `struct clk`). Dropping the guard releases the
/// reference only — it does not change hardware state; balance
/// `prepare_enable` explicitly or through the devm retirement cleanup.
#[derive(Clone)]
pub struct Clk {
    consumer: Arc<dyn ClkConsumer>,
}

impl Clk {
    /// Host adapters only: wraps an erased framework consumer. Drivers
    /// receive a `Clk` from the devm acquisition helpers, never construct
    /// one directly.
    pub fn new(consumer: Arc<dyn ClkConsumer>) -> Self {
        Self { consumer }
    }

    /// `clk_prepare` + `clk_enable`, propagating to the parent chain.
    ///
    /// # Errors
    ///
    /// Returns the framework's failure with propagated state rolled back.
    pub fn prepare_enable(&self) -> ResResult {
        self.consumer.prepare_enable()
    }

    /// `clk_disable` + `clk_unprepare` on the parent chain.
    pub fn disable_unprepare(&self) {
        self.consumer.disable_unprepare();
    }

    /// Current cached rate in Hz.
    pub fn get_rate(&self) -> u64 {
        self.consumer.get_rate()
    }

    /// Rounds a rate request without programming hardware.
    ///
    /// # Errors
    ///
    /// Returns the framework's failure when the request cannot be satisfied.
    pub fn round_rate(&self, hz: u64) -> ResResult<u64> {
        self.consumer.round_rate(hz)
    }

    /// Programs a new rate.
    ///
    /// # Errors
    ///
    /// Returns the framework's failure when the request cannot be satisfied.
    pub fn set_rate(&self, hz: u64) -> ResResult<u64> {
        self.consumer.set_rate(hz)
    }

    /// Whether the clock is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.consumer.is_enabled()
    }
}

impl core::fmt::Debug for Clk {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Clk")
            .field("rate_hz", &self.get_rate())
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

/// Host capability: resolve a clock connection to a reference.
///
/// `(provider_id, index)` is the device-tree `clocks` specifier pair; the
/// firmware layer turns a consumer's connection name into the pair.
///
/// # Errors
///
/// Implementations return [`ResError::NotFound`] when the provider has not
/// registered (or does not exist) and the framework's failure otherwise.
pub trait ClockOp: Sync {
    fn clk_get(&self, provider_id: u32, index: u32) -> ResResult<Clk>;
}
