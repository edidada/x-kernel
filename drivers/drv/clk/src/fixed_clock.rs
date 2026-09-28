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
//! publishes a single fixed-rate kclk output under its phandle. Consumers
//! that probed earlier and deferred on this phandle are woken by
//! [`driver_runtime::register_clk_provider`].
//!
//! This is the golden template for **provider drivers**: everything
//! is one file, registration is data, and the probe-to-wakeup handshake is
//! the framework's job.

use alloc::{boxed::Box, sync::Arc};

use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError,
    platform::{OfMatch, PlatformBus, PlatformId},
};
use kclk::{ClkBuilder, ClkCore, ClkLookup, FixedRate, ProviderId};

/// `struct of_device_id fixed_clock_of_match[]` equivalent.
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("fixed-clock"))];

/// One published fixed clock. Boot-static per the kclk model: the lookup is
/// leaked into `'static` at probe time and intentionally never freed — clock
/// controllers live for the kernel's lifetime (matching `of_clk_add_hw_provider`).
struct FixedClockLookup {
    provider_id: ProviderId,
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
        // `#clock-cells = <0>` nodes have no SoC output names.
        let _ = name;
        None
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

    fn probe(&self, dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<FixedClockData, ProbeError> {
        let firmware = dev
            .descriptor()
            .firmware
            .as_ref()
            .ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
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
            core: core.clone(),
        }));
        driver_runtime::register_clk_provider(lookup);

        log::info!("fixed-clock: published `{name}` @ {frequency} Hz (phandle {phandle:#x})");
        Ok(FixedClockData { _core: core })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, FixedClockData>) {
        // Clocks are boot-static; the lookup intentionally outlives the device.
    }
}

/// `module_platform_driver()` equivalent: register with the global domain.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new(
        "fixed-clock",
        IDS,
        FixedClockDriver,
    ));
}
