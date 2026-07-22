// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Glue layer for the dw_mshc SD card driver (RK3588 `sdmmc` node @ fe2c0000).
//!
//! This driver is a from-scratch port of the upstream Linux `dw_mmc` /
//! `dw_mmc-rockchip` controller behaviour, re-implemented in Rust for x-kernel.
//! See `drivers/block/src/sdmmc/` for the hardware logic layer and
//! `docs/ai/skills/linux-driver-port/examples/dw_mmc-walkthrough.md` for the
//! port rationale and the gap report.

use alloc::{boxed::Box, sync::Arc};

use driver_base::DeviceKind;
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject};

use crate::driver_registry::{BoxedDriver, firmware_specs::SDMMC};

/// Card interface clock the bootloader is assumed to have configured for the
/// `sdmmc` controller, in Hz. Used for CLKDIV math.
///
/// **Gap**: the real value should come from the DT `clock-frequency` property
/// via a clk (CRU) provider — x-kernel has no clk framework yet. This default
/// matches the common RK3588 SDK configuration and may need per-board tuning.
const SDMMC_BUS_HZ: u32 = 100_000_000;

struct SdmmcDriver;

impl DeviceDriver for SdmmcDriver {
    fn name(&self) -> &'static str {
        "sdmmc"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &SDMMC
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> driver_base::DriverResult<()> {
        let (vaddr, _) = crate::iomap_first_mmio(device.as_ref(), "sdmmc")?;
        let vaddr = vaddr.as_ptr() as usize;

        // SAFETY: `iomap_first_mmio` returned a kernel virtual address that
        // exclusively maps the device's first MMIO region (the dw_mshc register
        // block) and lives at least as long as `device`. `SdmmcDriver::new`
        // touches only this mapping during controller bring-up.
        let dev = match unsafe { block::sdmmc::SdmmcDriver::new(vaddr, SDMMC_BUS_HZ) } {
            Ok(d) => d,
            Err(e) => {
                log::error!(
                    "sdmmc: failed to initialize card at {:?}: {:?}",
                    device.location(),
                    e
                );
                return Err(e);
            }
        };
        kclass::publish_block(device, Box::new(dev))
    }
}

pub(super) fn descriptor() -> BoxedDriver {
    Arc::new(SdmmcDriver)
}
