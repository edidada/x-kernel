// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `fixed-clock` provider: publishes a DT fixed-rate clock into `kclk`.
//!
//! The node declares `#clock-cells` and a `clock-frequency`; the driver
//! registers under the node's firmware id so consumers' `clocks = <&node>`
//! specifiers resolve. The `ClkCore` node is owned by the binding and stays
//! alive for the kernel's lifetime, matching the boot-static provider
//! registration of Linux's `of_clk_init`.

use alloc::{boxed::Box, sync::Arc};

use driver_base::{DeviceKind, DriverError, DriverResult};
use kclk::{ClkBuilder, ClkCore, ClkIndex, ClkLookup, FixedRate, ProviderId, register_provider};
use kdevice::{BusTypeId, DeviceDriver, FirmwareMatchSpec, ProbeContext};

use crate::driver_registry::BoxedDriver;

static FIXED_CLOCK_MATCH: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "clk-fixed",
    dt_compatibles: &["fixed-clock"],
    acpi_ids: &[],
};

/// Provider driver for `fixed-clock` nodes.
struct FixedClockDriver;

impl DeviceDriver for FixedClockDriver {
    /// The registered `ClkCore` node: keeps the clock alive for the binding
    /// lifetime (providers are boot-static, so this is effectively forever).
    type Data = Arc<ClkCore>;

    fn name(&self) -> &'static str {
        "clk-fixed"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Bus
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn kdevice::DeviceMatcher {
        &FIXED_CLOCK_MATCH
    }

    fn probe_device(&self, ctx: &ProbeContext<'_>) -> DriverResult<Self::Data> {
        let node = ctx
            .device()
            .firmware_node()
            .ok_or(DriverError::InvalidInput)?;
        let frequency = of::clocks::frequency(&node).ok_or(DriverError::InvalidInput)?;
        let node_id = node.id().raw();
        let name = node.name().unwrap_or("fixed-clock");

        let core = ClkBuilder::new(name, FixedRate::new(frequency).into_ops())
            .build()
            .map_err(|_| DriverError::Io)?;

        let lookup: &'static FixedClockLookup = Box::leak(Box::new(FixedClockLookup {
            provider_id: node_id,
            core: core.clone(),
        }));
        register_provider(lookup);

        log::info!("clk-fixed: '{name}' registered at {frequency} Hz (provider {node_id:#x})");
        Ok(core)
    }
}

/// Single-output lookup registered under the provider node's firmware id.
struct FixedClockLookup {
    provider_id: ProviderId,
    core: Arc<ClkCore>,
}

impl ClkLookup for FixedClockLookup {
    fn provider_id(&self) -> ProviderId {
        self.provider_id
    }

    fn get_by_index(&self, index: ClkIndex) -> Option<Arc<ClkCore>> {
        (index == 0).then(|| self.core.clone())
    }
}

pub(super) fn descriptor() -> BoxedDriver {
    BoxedDriver::new(FixedClockDriver)
}
