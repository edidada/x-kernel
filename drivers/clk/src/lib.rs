// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! OS-neutral clock (`clk`) subsystem — Tier 0 (rate query).
//!
//! This crate extends the `device-res` capability pattern
//! ("Desc + Op Trait + RAII Handle + Provider") to clocks, so a driver can ask
//! "what is the rate of my `ciu` clock?" instead of hard-coding it. See
//! `docs/design.md` for the full layering and the Tier 0/1/2 roadmap.
//!
//! # Why a *multi-provider* registry
//!
//! Unlike MMIO/IRQ/DMA, where `device-res` installs a single global provider,
//! a clock tree has **many** producers — one per clock-output DT node (a SoC
//! CRU, an SCMI clock protocol, every `fixed-clock` crystal, ...). Each owns a
//! namespace of clock ids. A clock is resolved in two steps:
//!
//! 1. The **bus** pre-parses each device's DT `clocks` / `clock-names` at
//!    enumeration into `device_res::ClkDesc` (`name`, producer phandle,
//!    specifier cells), carried as a `ResourceDesc::Clock` resource — exactly as
//!    it pre-parses `reg`/`interrupts`.
//! 2. [`devm_clk_get`] finds that resource by name on the device and looks the
//!    provider up by phandle in the registry. The driver never touches the DT.
//!
//! # Tier 0 scope — read-only rate query
//!
//! Only [`ClkProvider::get_rate`] is exposed. This is enough to retire the
//! hard-coded clock-rate gaps in SoC drivers (the `sdmmc` `bus_hz` constant,
//! UART baud divisors, ...). Clock enable/disable, `set_rate`, reparenting and
//! rate-change propagation arrive in later tiers; their hooks are sketched on
//! the trait so the consumer API does not change shape when they land.
//!
//! # Platform neutrality
//!
//! This crate knows nothing about any SoC. Providers are registered either by
//! per-SoC glue (e.g. a Rockchip CRU driver) or by generic, cross-platform
//! provider implementations ([`FixedClockProvider`] for `compatible =
//! "fixed-clock"`, the RK3588 SCMI client in `kplat-aarch64`). The consumer
//! call `clk::devm_clk_get(device, "ciu")` is identical on every platform.

#![no_std]

extern crate alloc;

mod consumer;
mod providers;
mod registry;

use alloc::sync::Arc;

pub use consumer::devm_clk_get;
pub use providers::FixedClockProvider;
pub use registry::{ClkRegistry, register_provider};
use smallvec::SmallVec;

/// Largest number of clock-specifier cells a provider's `clocks` entry can
/// carry (besides the leading phandle). Two covers RK3588 (`#clock-cells = <1>`)
/// with headroom for clock-framework providers that append a flags word.
pub(crate) const MAX_SPECIFIER_CELLS: usize = 2;

/// A clock producer: a DT node carrying `#clock-cells` that answers rate
/// queries (and, in later tiers, enable / `set_rate`) for the clocks it owns.
///
/// `specifier` is the sequence of u32 cells following the provider's phandle in
/// a consumer's `clocks = <&provider N M ...>` property. For a `#clock-cells =
/// <0>` provider (e.g. `fixed-clock`) it is empty.
///
/// Implementations must be `Send + Sync`: the registry is a global shared by
/// every CPU and a registration outlives the probe that made it.
/// Provider-operation errors. Tier 0 only ever produces `Unsupported`; the
/// richer variants are declared now so the trait shape is stable when Tier 1
/// providers (real enable / set_rate implementations) arrive — they populate
/// the variants instead of reshaping the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClkError {
    /// This provider cannot perform the operation for the given specifier
    /// (Tier 0's default for every mutating op; also a real Tier 1 answer when
    /// a clock genuinely has no enable gate / no rate divider).
    Unsupported,
    /// The specifier does not name a clock this provider owns.
    BadSpecifier,
    /// The hardware refused the operation (gate stuck, PLL did not lock, ...).
    Hardware,
}

/// A clock producer: a DT node carrying `#clock-cells` that answers rate
/// queries (and, in later tiers, enable / `set_rate`) for the clocks it owns.
///
/// `specifier` is the sequence of u32 cells following the provider's phandle in
/// a consumer's `clocks = <&provider N M ...>` property. For a `#clock-cells =
/// <0>` provider (e.g. `fixed-clock`) it is empty.
///
/// **API stability:** every method is declared now with a Tier 0 default.
/// Tier 1 providers override the mutating methods; the method set itself does
/// not change, so adapters written against this trait keep compiling.
///
/// Implementations must be `Send + Sync`: the registry is a global shared by
/// every CPU and a registration outlives the probe that made it.
pub trait ClkProvider: Send + Sync {
    /// Current rate of the selected clock in Hz, or `None` if the provider
    /// cannot describe this specifier (unknown id, not powered, ...).
    fn get_rate(&self, specifier: &[u32]) -> Option<u64>;

    /// Enable the clock (refcount-managed in Tier 1). Tier 0 default: the
    /// clock is assumed already enabled by firmware, so this is a no-op.
    fn enable(&self, _specifier: &[u32]) -> Result<(), ClkError> {
        Ok(())
    }

    /// Disable the clock. Tier 0 default: refuses — firmware-enabled clocks
    /// must not be turned off by a driver that cannot turn them back on.
    fn disable(&self, _specifier: &[u32]) -> Result<(), ClkError> {
        Err(ClkError::Unsupported)
    }

    /// Set the rate in Hz, returning the rounded rate the hardware actually
    /// selected. Tier 0 default: unsupported (rate is read-only).
    fn set_rate(&self, _specifier: &[u32], _rate_hz: u64) -> Result<u64, ClkError> {
        Err(ClkError::Unsupported)
    }

    /// Whether the clock is currently enabled, if the provider can tell.
    /// Tier 0 default: unknown.
    fn is_enabled(&self, _specifier: &[u32]) -> Option<bool> {
        None
    }
}

/// A resolved clock: the provider that owns it plus the specifier that selects
/// the clock within that provider.
///
/// Built by [`devm_clk_get`]. Tier 0 carries no enable refcount, so the handle
/// is cheap to hold across a probe and to drop when the device is removed.
pub struct Clk {
    provider: Arc<dyn ClkProvider>,
    specifier: SmallVec<[u32; MAX_SPECIFIER_CELLS]>,
}

impl Clk {
    /// Crate-internal constructor; consumers obtain a `Clk` via [`devm_clk_get`].
    pub(crate) fn new(
        provider: Arc<dyn ClkProvider>,
        specifier: SmallVec<[u32; MAX_SPECIFIER_CELLS]>,
    ) -> Self {
        Self {
            provider,
            specifier,
        }
    }

    /// Current rate of this clock in Hz, or `None` if unresolved.
    pub fn rate(&self) -> Option<u64> {
        self.provider.get_rate(&self.specifier)
    }

    /// Enable this clock. See [`ClkProvider::enable`].
    pub fn enable(&self) -> Result<(), ClkError> {
        self.provider.enable(&self.specifier)
    }

    /// Disable this clock. See [`ClkProvider::disable`].
    pub fn disable(&self) -> Result<(), ClkError> {
        self.provider.disable(&self.specifier)
    }

    /// Set this clock's rate in Hz, returning the rounded rate. See
    /// [`ClkProvider::set_rate`].
    pub fn set_rate(&self, rate_hz: u64) -> Result<u64, ClkError> {
        self.provider.set_rate(&self.specifier, rate_hz)
    }

    /// Whether this clock is enabled, if known. See [`ClkProvider::is_enabled`].
    pub fn is_enabled(&self) -> Option<bool> {
        self.provider.is_enabled(&self.specifier)
    }
}

#[cfg(unittest)]
mod tests {
    //! Unit tests for the registry routing, the `Clk` handle delegation, and the
    //! `devm_clk_get` device-resource entry point. The bus-layer DT parsing
    //! (`clocks`/`clock-names` → `ClkDesc`) lives in `khal` and needs a real
    //! DTB, so it is exercised on hardware at probe time, not here. These tests
    //! pin the framework contract — providers are routed by phandle, the
    //! specifier reaches the provider, and `devm_clk_get` resolves a named clock
    //! on a device. Routing/shadowing/isolation of the registry itself are
    //! covered in `registry::tests`; here we exercise only what `lib` owns:
    //! the `FixedClockProvider` rate, the `Clk` handle, and `devm_clk_get`.
    //! Tests use distinct provider names so the process-global registry needs
    //! no reset between them.

    use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

    use device_res::{ClkDesc, DeviceResource, ResourceDesc};
    use unittest::def_test;

    use crate::{ClkProvider, FixedClockProvider, devm_clk_get, register_provider};

    /// Echoes the first specifier cell as `cell * 1 MHz`, modelling a provider
    /// that keys its clocks by an id cell (`#clock-cells = <1>`).
    struct IdClock;
    impl ClkProvider for IdClock {
        fn get_rate(&self, specifier: &[u32]) -> Option<u64> {
            specifier.first().map(|&id| id as u64 * 1_000_000)
        }
    }

    #[def_test]
    fn fixed_clock_provider_returns_its_rate() {
        let provider = FixedClockProvider::new(24_000_000);
        assert_eq!(provider.get_rate(&[]), Some(24_000_000));
    }

    /// A minimal `DeviceResource` whose resources are fixed at construction —
    /// used to exercise `devm_clk_get` without a real device or bus.
    struct ClockDevice {
        resources: Vec<ResourceDesc>,
    }
    impl DeviceResource for ClockDevice {
        fn resources(&self) -> &[ResourceDesc] {
            &self.resources
        }

        fn register_cleanup(&self, _cleanup: Box<dyn FnOnce() + Send>) {}
    }

    #[def_test]
    fn devm_clk_get_resolves_a_named_clock() {
        // Mirrors RK3588 sdmmc: `ciu = <&scmi_clk 9>` → provider "scmi_clk", id 9.
        register_provider("scmi_clk", Arc::new(IdClock));

        let device = ClockDevice {
            resources: vec![ResourceDesc::Clock(ClkDesc::new(b"ciu", "scmi_clk", &[9]))],
        };

        let clk = devm_clk_get(&device, "ciu").expect("ciu clock present");
        assert_eq!(clk.rate(), Some(9_000_000));

        // Unknown consumer name → no matching resource → InvalidResource.
        assert!(devm_clk_get(&device, "biu").is_err());

        // Known consumer name but a provider nobody registered → NoProvider.
        let unbound = ClockDevice {
            resources: vec![ResourceDesc::Clock(ClkDesc::new(
                b"ciu",
                "never_registered",
                &[1],
            ))],
        };
        assert!(devm_clk_get(&unbound, "ciu").is_err());
    }
}
