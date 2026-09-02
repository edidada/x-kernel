// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic fixed-clock platform driver.
//!
//! A clock controller is a platform device like any other: its DT node
//! (`compatible = "fixed-clock"`) is enumerated by the platform bus backend,
//! matched to this driver, and probed. The probe registers the clock with
//! the kclk provider registry under the node's DT phandle, so consumers
//! resolve it via `clocks = <&phandle>` references (`ClkSpec::by_phandle_index`)
//! exactly like MMIO and IRQ resources.
//!
//! The platform backend registers clock providers (nodes with
//! `#clock-cells`) before their consumers, so one probe pass satisfies the
//! dependency order — the same guarantee Linux's `of_clk_init` +
//! `EPROBE_DEFER` combo provides.
//!
//! SoC-specific controllers (PLL/mux/divider/gate trees) have their own
//! compatible strings and carry an MMIO register window; their drivers
//! follow the same pattern in their own `probe_device`.

use alloc::{boxed::Box, sync::Arc};

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

        // Discover this node's (phandle, frequency) from the DT. The DT
        // order and the probe order agree because the platform backend
        // registers clock providers first; for multi-clock trees the node
        // identity routing is done by the SoC controller driver instead.
        let Some((phandle, rate)) = first_fixed_clock_node() else {
            log::warn!("clk-fixed-clock: no clock-frequency in DT");
            return Ok(());
        };

        // Register under the DT phandle so `clocks = <&phandle>` references
        // resolve. The table is leaked: the provider registry holds a
        // 'static reference for the lifetime of the boot.
        let table: &'static kclk::ClkTable = Box::leak(Box::new(kclk::ClkTable::new(phandle)));
        let output_name = fixed_clock_output_name().unwrap_or("fixed-clock");
        let branches = [kclk::ClkBranch::fixed_rate(0, output_name, u64::from(rate))];

        // NullClkReg: a fixed clock is purely computational.
        let reg: Arc<dyn kclk::ClkRegAccess> = Arc::new(kclk::NullClkReg);
        match kclk::register_table(table, reg, &branches) {
            Ok(()) => {
                kclk::register_provider(table);
                log::warn!(
                    "clk-fixed-clock: registered {output_name} ({rate} Hz, phandle 0x{phandle:x})"
                );
            }
            Err(e) => log::warn!("clk-fixed-clock: failed to register {output_name}: {e:?}"),
        }

        Ok(())
    }
}

/// Walk the DT for the first `fixed-clock` node's phandle + frequency.
fn first_fixed_clock_node() -> Option<(u32, u32)> {
    let fdt = of::fdt()?;
    for node in fdt.all_nodes() {
        if node.compatibles().any(|c| c == "fixed-clock")
            && let Some(freq) = node.property_u32("clock-frequency")
        {
            // QEMU-virt style nodes carry `phandle`; fall back to
            // `linux,phandle` for older DT generators.
            let phandle = node
                .property_u32("phandle")
                .or_else(|| node.property_u32("linux,phandle"))?;
            return Some((phandle, freq));
        }
    }
    None
}

/// The fixed-clock node's `clock-output-names` (preferred) or node name.
fn fixed_clock_output_name() -> Option<&'static str> {
    let fdt = of::fdt()?;
    for node in fdt.all_nodes() {
        if node.compatibles().any(|c| c == "fixed-clock") {
            // clock-output-names is a NUL-separated string list; take the
            // first entry.
            let prop = node.property("clock-output-names")?;
            let name = prop.value.split(|&b| b == 0).next()?;
            return core::str::from_utf8(name).ok();
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
