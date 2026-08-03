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
/// **Gap / bring-up note**: the authoritative source is the CIU clock rate,
/// which Linux reads via the clk framework (`clk_get_rate(ciu_clk)`) and
/// Rockchip further divides by `RK3288_CLKGEN_DIV` (=2). x-kernel has no clk
/// framework, and the Orange Pi 5 Plus `mmc@fe2c0000` DT node has **no**
/// `clock-frequency` property (only `max-frequency = <150_000_000>`, which is
/// the card-clock cap, NOT the CIU rate — do not use it for CLKDIV math).
///
/// This constant is therefore an empirical fallback. 50 MHz is what U-Boot
/// reports for the card on this exact board (`mmc info` → Bus Speed 50000000,
/// "SD High Speed (50MHz)"), and is logged at probe so a mismatch is visible.
/// If card identification fails with CMDx RTO, suspect this value first and
/// cross-check against U-Boot's reported bus speed.
const SDMMC_BUS_HZ: u32 = 50_000_000;

/// Resolve the dw_mshc CIU input rate via the `clk` framework, falling back to
/// the empirical U-Boot value when no provider answers yet.
///
/// The bus pre-parses `clocks`/`clock-names` into the device's resources, so we
/// resolve `ciu` through `clk::devm_clk_get` directly — no DT node lookup. On
/// RK3588 `ciu` is SCMI-owned; `CLOCK_RATE_GET` returns the CIU reference rate
/// (~49.5 MHz), used verbatim as `bus_hz` (no further division). If the clock
/// can't be resolved or has no rate we keep the 50 MHz value U-Boot reports.
fn resolve_ciu_rate(device: &kdevice::DeviceObject) -> u32 {
    let rate = clk::devm_clk_get(device, "ciu")
        .ok()
        .and_then(|clk| clk.rate());
    match rate {
        Some(hz) => {
            log::info!("sdmmc: ciu clock resolved via clk framework: {} Hz", hz);
            hz as u32
        }
        None => {
            log::warn!(
                "sdmmc: ciu clock unresolved; falling back to {} Hz",
                SDMMC_BUS_HZ
            );
            SDMMC_BUS_HZ
        }
    }
}

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

        let bus_hz = resolve_ciu_rate(device.as_ref());

        // SAFETY: `iomap_first_mmio` returned a kernel virtual address that
        // exclusively maps the device's first MMIO region (the dw_mshc register
        // block) and lives at least as long as `device`. `SdmmcDriver::new`
        // touches only this mapping during controller bring-up.
        let dev = match unsafe { block::sdmmc::SdmmcDriver::new(vaddr, bus_hz) } {
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
