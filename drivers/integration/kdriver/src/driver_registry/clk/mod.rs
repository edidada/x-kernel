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

use alloc::{boxed::Box, format, sync::Arc};

use driver_base::{DeviceKind, DriverError, DriverResult};
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

        // Read the registration parameters from this device's own firmware
        // node — the Linux `dev.of_node` pattern. No global DT walk: the
        // node belongs to exactly this device, so distinct fixed-clock
        // nodes sharing the compatible route to their own parameters.
        let node = device.firmware_node().ok_or(DriverError::InvalidInput)?;
        let rate = u64::from(
            node.read_u32("clock-frequency")
                .ok_or(DriverError::InvalidInput)?,
        );
        // `phandle` is the canonical spelling; `linux,phandle` is the
        // legacy alias some older generators emit.
        let phandle = node
            .read_u32("phandle")
            .or_else(|| node.read_u32("linux,phandle"))
            .ok_or(DriverError::InvalidInput)?;

        // Register under the DT phandle so `clocks = <&phandle>` references
        // resolve. The table is leaked: the provider registry holds a
        // 'static reference for the lifetime of the boot.
        let table: &'static kclk::ClkTable = Box::leak(Box::new(kclk::ClkTable::new(phandle)));
        // DT does not require clock-output-names; derive a unique fallback
        // from the phandle so two anonymous fixed clocks cannot collide in
        // the by-name registry.
        let fallback_name: &'static str =
            Box::leak(format!("fixed@0x{phandle:x}").into_boxed_str());
        let output_name = node.read_str("clock-output-names").unwrap_or(fallback_name);
        let branches = [kclk::ClkBranch::fixed_rate(0, output_name, rate)];

        // NullClkReg: a fixed clock is purely computational.
        let reg: Arc<dyn kclk::ClkRegAccess> = Arc::new(kclk::NullClkReg);
        kclk::register_table(table, reg, &branches).map_err(map_clk_probe_error)?;
        kclk::register_provider(table);
        log::debug!("clk-fixed-clock: registered {output_name} ({rate} Hz, phandle 0x{phandle:x})");

        Ok(())
    }
}

fn map_clk_probe_error(error: kclk::ClkError) -> DriverError {
    match error {
        kclk::ClkError::NoProvider => DriverError::WouldBlock,
        kclk::ClkError::Io => DriverError::Io,
        kclk::ClkError::Busy | kclk::ClkError::Aborted => DriverError::ResourceBusy,
        kclk::ClkError::NotSupported => DriverError::Unsupported,
        kclk::ClkError::NotFound
        | kclk::ClkError::InvalidRate
        | kclk::ClkError::InvalidParent
        | kclk::ClkError::BadState => DriverError::InvalidInput,
    }
}

/// Factory for the built-in driver registry.
pub fn fixed_clock_descriptor() -> crate::driver_registry::BoxedDriver {
    Arc::new(FixedClockDriver)
}

const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[fixed_clock_descriptor];

pub fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    crate::driver_registry::register_factories(registrar, DRIVER_FACTORIES);
}
