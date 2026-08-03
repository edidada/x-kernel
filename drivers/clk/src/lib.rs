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
pub use providers::{FixedClockProvider, register_fixed_clocks};
pub use registry::register_provider;
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
pub trait ClkProvider: Send + Sync {
    /// Current rate of the selected clock in Hz, or `None` if the provider
    /// cannot describe this specifier (unknown id, not powered, ...).
    fn get_rate(&self, specifier: &[u32]) -> Option<u64>;

    // Tier 1+ — deliberately not yet implemented. Declared here as a reminder
    // that the consumer API is designed to grow these without reshaping:
    //   fn enable(&self, specifier: &[u32]) -> ClkResult;
    //   fn disable(&self, specifier: &[u32]);
    //   fn set_rate(&self, specifier: &[u32], rate_hz: u64) -> ClkResult<u64>;
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
}

#[cfg(unittest)]
mod tests {
    //! Unit tests for the registry routing, the `Clk` handle delegation, and the
    //! `devm_clk_get` device-resource entry point. The bus-layer DT parsing
    //! (`clocks`/`clock-names` → `ClkDesc`) lives in `khal` and needs a real
    //! DTB, so it is exercised on hardware at probe time, not here. These tests
    //! pin the framework contract — providers are routed by phandle, the
    //! specifier reaches the provider, and `devm_clk_get` resolves a named clock
    //! on a device.

    use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

    use device_res::{ClkDesc, DeviceResource, ResourceDesc};
    use kspin::SpinNoIrq;
    use unittest::def_test;

    use crate::{
        Clk, ClkProvider, FixedClockProvider, devm_clk_get, register_provider,
        registry::{reset_registry, try_provider},
    };

    /// Serialises tests that touch the shared global registry.
    static TEST_SERIAL: SpinNoIrq<()> = SpinNoIrq::new(());

    /// Returns a fixed rate regardless of specifier.
    struct FixedRate(u64);
    impl ClkProvider for FixedRate {
        fn get_rate(&self, _specifier: &[u32]) -> Option<u64> {
            Some(self.0)
        }
    }

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

    #[def_test]
    fn registry_routes_by_phandle_and_specifier() {
        let _guard = TEST_SERIAL.lock();
        reset_registry();

        register_provider(0x10, Arc::new(FixedRate(50_000_000)));
        register_provider(0x20, Arc::new(IdClock));

        let fixed = try_provider(0x10).expect("phandle 0x10 registered");
        assert_eq!(fixed.get_rate(&[]), Some(50_000_000));

        let keyed = try_provider(0x20).expect("phandle 0x20 registered");
        assert_eq!(keyed.get_rate(&[9]), Some(9_000_000));

        assert!(
            try_provider(0xdead).is_none(),
            "unregistered phandle resolves"
        );
    }

    #[def_test]
    fn reregistration_shadows_the_earlier_provider() {
        let _guard = TEST_SERIAL.lock();
        reset_registry();

        register_provider(0x30, Arc::new(FixedRate(1)));
        register_provider(0x30, Arc::new(FixedRate(2)));

        assert_eq!(
            try_provider(0x30).unwrap().get_rate(&[]),
            Some(2),
            "latest registration for a phandle must win",
        );
    }

    #[def_test]
    fn clk_handle_delegates_rate_to_its_provider() {
        let _guard = TEST_SERIAL.lock();
        reset_registry();

        register_provider(0x2c2, Arc::new(IdClock));
        let provider = try_provider(0x2c2).unwrap();
        let clk = Clk::new(provider, smallvec::smallvec![0x2c2]);

        assert_eq!(clk.rate(), Some(0x2c2_u64 * 1_000_000));
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
        let _guard = TEST_SERIAL.lock();
        reset_registry();
        register_provider(0x0e, Arc::new(IdClock));

        // Mirrors RK3588 sdmmc: `ciu = <&scmi_clk 9>` → phandle 0x0e, id 9.
        let device = ClockDevice {
            resources: vec![ResourceDesc::Clock(ClkDesc::new(b"ciu", 0x0e, &[9]))],
        };

        let clk = devm_clk_get(&device, "ciu").expect("ciu clock present");
        assert_eq!(clk.rate(), Some(9_000_000));

        // Unknown name → no matching resource → InvalidResource.
        assert!(devm_clk_get(&device, "biu").is_err());

        // Known name but no provider registered for its phandle → NoProvider.
        let unbound = ClockDevice {
            resources: vec![ResourceDesc::Clock(ClkDesc::new(b"ciu", 0xff, &[1]))],
        };
        assert!(devm_clk_get(&unbound, "ciu").is_err());
    }
}
