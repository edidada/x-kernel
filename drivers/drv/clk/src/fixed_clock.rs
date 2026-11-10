// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `fixed-clock` provider driver (driver-core model).
//!
//! The Linux `of_fixed_factor_clk`-style leaf: a DT node like
//!
//! ```dts
//! clk24m: clock-24m {
//!     compatible = "fixed-clock";
//!     #clock-cells = <0>;
//!     clock-frequency = <24000000>;
//! };
//! ```
//!
//! publishes a single fixed-rate kclk output under its phandle through the
//! generic provider door (`dev.env().provide::<ClkProvider>`). Consumers that
//! probed earlier and deferred on this phandle are woken by that publication
//! — the driver names no register function, only its provider class.
//!
//! This is the golden template for **provider drivers**: everything
//! is one file, registration is data, and the probe-to-wakeup handshake is
//! the framework's job.

use alloc::{boxed::Box, sync::Arc};

use driver_base::{Device, DriverOps, OfMatch, PlatformBus, PlatformId, ProbeError, RuntimeError};
use kclk::{ClkBuilder, ClkCore, ClkLookup, FixedRate, ProviderId};

/// `struct of_device_id fixed_clock_of_match[]` equivalent.
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("fixed-clock"))];

/// One published fixed clock. Boot-static per the kclk model: the lookup is
/// leaked into `'static` at probe time and intentionally never freed — clock
/// controllers live for the kernel's lifetime (matching `of_clk_add_hw_provider`).
struct FixedClockLookup {
    provider_id: ProviderId,
    /// The output's name (`clock-output-names`, or the phandle-derived
    /// fallback). Cross-provider parent resolution resolves branch parents
    /// by NAME against the global registry, so a fixed input must answer
    /// its name here — `xin24m` is a fixed-clock output and the CRU table
    /// lists it as a parent.
    name: &'static str,
    core: Arc<ClkCore>,
}

impl ClkLookup for FixedClockLookup {
    fn provider_id(&self) -> ProviderId {
        self.provider_id
    }

    fn get_by_index(&self, index: kclk::ClkIndex) -> Option<Arc<ClkCore>> {
        (index == 0).then(|| self.core.clone())
    }

    fn get_by_name(&self, name: &str) -> Option<Arc<ClkCore>> {
        (name == self.name).then(|| self.core.clone())
    }
}

/// Driver-private data (the drvdata): holds the clock alive.
pub struct FixedClockData {
    _core: Arc<ClkCore>,
}

/// The driver ops.
pub struct FixedClockDriver;

impl DriverOps<PlatformBus> for FixedClockDriver {
    type Data = FixedClockData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<FixedClockData, ProbeError> {
        // The view borrows the device; bind it so the node reference below
        // outlives the temporary.
        let fw = dev.fw();
        let firmware = fw.node().ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        let frequency = firmware
            .clock_frequency
            .ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        let phandle = firmware
            .phandle
            .ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;

        let name: &'static str = firmware.clock_output_name.unwrap_or("fixed-clock");

        let core = ClkBuilder::new(name, FixedRate::new(frequency as u64).into_ops())
            .build()
            .map_err(|_| ProbeError::Fatal(RuntimeError::Io))
            .inspect_err(|_| log::warn!("fixed-clock: builder failed for `{name}`"))?;

        // SAFETY-of-model note: leak is intentional, see `FixedClockLookup`.
        let lookup: &'static FixedClockLookup = Box::leak(Box::new(FixedClockLookup {
            provider_id: phandle,
            name,
            core: core.clone(),
        }));
        // Publish LAST: registration wakes consumers parked on this phandle
        // and cannot be undone by a later `Err`. The framework maps a
        // duplicate/invalid publication to a fatal probe error.
        dev.env()
            .provide::<driver_host::ClkProvider>(phandle, lookup)?;

        log::info!("fixed-clock: published `{name}` @ {frequency} Hz (phandle {phandle:#x})");
        Ok(FixedClockData { _core: core })
    }

    fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut FixedClockData) {
        // Clocks are boot-static; the lookup intentionally outlives the device.
    }
}

driver_base::declare_driver!(PlatformBus, FixedClockDriver, "fixed-clock", IDS);
