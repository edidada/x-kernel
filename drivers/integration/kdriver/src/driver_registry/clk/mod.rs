// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic fixed-clock platform driver.
//!
//! A clock controller is a platform device like any other: its DT node
//! (`compatible = "fixed-clock"`) is enumerated by the platform bus backend,
//! matched to this driver, and probed. The probe registers the clock with
//! the kclk provider registry.
//!
//! This establishes the device-model path for clock controllers, replacing
//! the manual `init_platform_clocks()` platform-init call. SoC-specific
//! controllers (PLL/mux/divider/gate trees) have their own compatible
//! strings and register via [`kclk::register_table`] in their own probe,
//! following the same pattern.

use alloc::sync::Arc;

use driver_base::{DeviceKind, DriverResult};
use kdevice::{
    BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject, FirmwareMatchSpec, MatchResult, priority,
};

/// Match DT `fixed-clock` nodes.
struct FixedClockMatcher;

static CLOCK_MATCH: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "clk-fixed-clock",
    dt_compatibles: &["fixed-clock"],
    acpi_ids: &[],
};

impl DeviceMatcher for FixedClockMatcher {
    fn matches(&self, desc: &kdevice::DeviceDesc) -> MatchResult {
        if let kdevice::DeviceIdentity::Platform(plat) = desc.identity()
            && let Some(id) = plat.firmware_id
            && CLOCK_MATCH.match_dt(id).is_some()
        {
            return MatchResult::Match {
                priority: priority::GENERIC,
            };
        }
        MatchResult::NoMatch
    }

    fn firmware_spec(&self) -> Option<&FirmwareMatchSpec> {
        Some(&CLOCK_MATCH)
    }
}

static MATCHER: FixedClockMatcher = FixedClockMatcher;

/// The fixed-clock table. Re-registration replaces; one active provider.
static TABLE: kclk::ClkTable = kclk::ClkTable::new(0xF1C0);

/// Generic fixed-clock platform driver.
pub struct FixedClockDriver;

impl DeviceDriver for FixedClockDriver {
    fn name(&self) -> &'static str {
        "clk-fixed-clock"
    }

    fn device_kind(&self) -> DeviceKind {
        // A clock controller is infrastructure, not a char/block/net device.
        // Bus is the closest existing kind for service-providing devices.
        DeviceKind::Bus
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &MATCHER
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        // A fixed-clock node has no MMIO; its rate is a DT property. If it
        // has MMIO, it is a full controller — let a specific driver claim it.
        if device.first_mmio().is_some() {
            return Ok(());
        }

        // Read the clock-frequency from the DT (walked via the of crate).
        let Some(rate) = first_fixed_clock_hz() else {
            log::warn!("clk-fixed-clock: no clock-frequency found");
            return Ok(());
        };

        let name = "uart-ref";
        let branches = [kclk::ClkBranch::fixed_rate(0, name, u64::from(rate))];

        // NullClkReg: a fixed clock is purely computational.
        let reg: Arc<dyn kclk::ClkRegAccess> = Arc::new(kclk::NullClkReg);
        match kclk::register_table(&TABLE, reg, &branches) {
            Ok(()) => {
                kclk::register_provider(&TABLE);
                log::info!("clk-fixed-clock: registered {name} ({rate} Hz)");
            }
            Err(e) => log::warn!("clk-fixed-clock: failed to register {name}: {e:?}"),
        }

        Ok(())
    }
}

/// Walk the DT for the first `fixed-clock` node's `clock-frequency`.
fn first_fixed_clock_hz() -> Option<u32> {
    let fdt = of::fdt()?;
    for node in fdt.all_nodes() {
        if node.compatibles().any(|c| c == "fixed-clock")
            && let Some(freq) = node.property_u32("clock-frequency")
        {
            return Some(freq);
        }
    }
    None
}

/// Factory for the built-in driver registry.
pub fn fixed_clock_descriptor() -> crate::driver_registry::BoxedDriver {
    Arc::new(FixedClockDriver)
}

const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[fixed_clock_descriptor];

pub fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    crate::driver_registry::register_factories(registrar, DRIVER_FACTORIES);
}
