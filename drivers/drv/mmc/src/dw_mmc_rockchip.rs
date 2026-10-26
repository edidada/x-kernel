// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Rockchip RK3588 SDMMC host driver (Linux `dw_mmc-rockchip.c` shape).
//!
//! The SoC glue: platform-bus probe resolves the controller's clocks and
//! DMA windows through the device resource doors, brings up a
//! [`DwMmcHost`], runs card detection through the subsystem's SD core,
//! and adds the discovered card to the domain on the card bus —
//! Linux's `mmc_add_host` + `mmc_detect_card` + `mmc_add_card` folded
//! into one probe.
//!
//! Clocks: `ciu` (`SCMI_CCLK_SD`) and `biu` (`SCMI_HCLK_SD`) come from
//! the SCMI clock provider; UHS-I is out of scope, so the driver never
//! reprograms the upstream rate — the controller divider covers
//! 400 kHz init → 25 MHz default → 50 MHz HS.

use alloc::sync::Arc;

use driver_base::{
    Bus, Device, DeviceDescriptor, DriverOps, IoMap, OfMatch, PlatformBus, PlatformId, ProbeError,
    RuntimeError,
};
use subsys_mmc::{
    BusWidth, DetectContext, MmcError,
    bus::{MmcBus, MmcCardData},
    sd,
};

use super::dw_mmc::{DwMmcHost, IDMAC_RING_BYTES, IDMAC_WINDOW_BYTES, IdmacBuffers};

/// `static const struct of_device_id dw_mmc_rockchip_match[]`.
static IDS: &[PlatformId] = &[
    PlatformId::Of(OfMatch::compatible("rockchip,rk3588-dw-mshc")),
    // The rk3588 node's secondary compatible — the generic dw_mshc
    // fallback Linux's of_device_id table matches the same way.
    PlatformId::Of(OfMatch::compatible("rockchip,rk3288-dw-mshc")),
];

/// Driver-private data: one reference to the shared host, which owns the
/// DMA mappings (card consumers hold Arc clones; see the dw_mmc note).
pub struct DwMmcRockchipData {
    _host: Arc<DwMmcHost>,
}

/// Host slot index allocator (`mmc_host->index`): probe order defines
/// the slot number used for disk naming and DEVPATH components.
static NEXT_SLOT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// The platform driver.
pub struct DwMmcRockchipDriver;

impl DriverOps<PlatformBus> for DwMmcRockchipDriver {
    type Data = DwMmcRockchipData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<DwMmcRockchipData, ProbeError> {
        let regs: IoMap = dev.res().iomap(0)?;
        let info = dev.info();
        let slot = NEXT_SLOT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let fifo_depth = dev.fw().u32("fifo-depth").unwrap_or(0x100);
        let max_frequency = dev.fw().u32("max-frequency").unwrap_or(50_000_000);

        // Clocking, the board-verified tgoskits/U-Boot shape: the CCLK_SD
        // source and HCLK_SD gate sit behind the Rockchip bl31 blob, whose
        // SMC entry does not serve the mainline SCMI clock protocol
        // (probing it costs three round trips and a HARDWARE_ERROR; see
        // drv/clk/scmi_clock for the full investigation), and the source
        // rate registers live in the Secure CRU — reading them from EL1
        // takes a data abort (verified on the board). So the driver does
        // NOT touch the clock source at all: the CIU input rate is the
        // board fact MMC_CIU_HZ (default 50 MHz, what the boot stages
        // leave programmed for the SD domain), and the DWMMC internal
        // divider covers 400 kHz init up to the 50 MHz HS ceiling.
        let ciu_hz = u32::try_from(kbuild_config::MMC_CIU_HZ)
            .map_err(|_| ProbeError::Fatal(RuntimeError::Invalid))?;
        if ciu_hz == 0 {
            log::warn!("dwmmc-rocks: MMC_CIU_HZ is unset; slot {slot} cannot program its divider");
            return Err(ProbeError::Fatal(RuntimeError::Invalid));
        }
        log::info!(
            "dwmmc-rocks: slot {slot} at {:#x}, ciu {ciu_hz} Hz, fifo {fifo_depth}, max \
             {max_frequency} Hz",
            regs.region().base
        );

        // One coherent descriptor page + one bounce window for the internal
        // DMA engine; both must stay addressable by 32-bit descriptors, so the
        // ring/window pair is only installed when the allocation lands below
        // 4 GiB (otherwise the host falls back to PIO).
        let idmac = match (
            dev.res().alloc_coherent(IDMAC_RING_BYTES, IDMAC_RING_BYTES),
            dev.res().alloc_coherent(IDMAC_WINDOW_BYTES, 4096),
        ) {
            (Ok(ring), Ok(window)) => match IdmacBuffers::new(ring, window) {
                Some(buffers) => Some(buffers),
                None => {
                    log::warn!(
                        "dwmmc-rocks: slot {slot}: IDMAC buffers are not below 4 GiB; using PIO"
                    );
                    None
                }
            },
            _ => {
                log::warn!("dwmmc-rocks: slot {slot}: no coherent memory for IDMAC; using PIO");
                None
            }
        };

        let host = Arc::new(
            DwMmcHost::new(
                regs,
                ciu_hz,
                fifo_depth,
                slot,
                card_clock_ceiling(max_frequency),
                bus_width_ceiling(),
                idmac,
            )
            .map_err(host_error)?,
        );

        // Card detection (Linux mmc_detect_card): no card is a clean
        // outcome — the slot stays claimed for a future re-scan.
        let detect = DetectContext {
            host: host.as_ref(),
            slot,
        };
        match sd::detect_card(&detect, host.clone()) {
            Ok(card) => {
                add_card_to_domain(info.id(), &host, card)?;
            }
            Err(MmcError::NoCard) => {
                log::info!("dwmmc-rocks: slot {slot}: no card present");
            }
            Err(err) => {
                log::warn!("dwmmc-rocks: slot {slot}: card detection failed: {err:?}");
            }
        }

        Ok(DwMmcRockchipData { _host: host })
    }
}

/// Publish the card onto the card bus (Linux `mmc_add_card`): one bus
/// instance per host, the card descriptor parented to the host device so
/// host removal cascades the card.
fn add_card_to_domain(
    host_dev: driver_base::DeviceId,
    host: &Arc<DwMmcHost>,
    card: MmcCardData,
) -> Result<(), ProbeError> {
    let domain = driver_base::global_domain();
    let instance = domain
        .add_bus_instance(
            MmcBus::ID,
            Some(host_dev),
            &alloc::format!("mmc{}", card.slot),
        )
        .map_err(|_| ProbeError::Fatal(RuntimeError::Busy))?;
    let descriptor = DeviceDescriptor::new(<MmcBus as Bus>::ID, alloc::boxed::Box::new(card))
        .with_bus_instance(instance)
        .with_parent(Some(host_dev));
    match domain.add_device(descriptor) {
        Ok(id) => {
            log::info!(
                "dwmmc-rocks: card {id:?} added on bus instance mmc{}",
                host.slot()
            );
            Ok(())
        }
        Err(err) => {
            log::warn!("dwmmc-rocks: card add rejected: {err:?}");
            Err(ProbeError::Fatal(RuntimeError::Invalid))
        }
    }
}

/// Requested card-clock ceiling (Linux `mmc->f_max`): the DT
/// `max-frequency` and the board fact `MMC_MAX_CARD_HZ`, whichever is
/// lower. 0 (either side) means "no vote from that side".
///
/// The SD clock domain sits behind the firmware, so this driver cannot
/// measure the rate it is dividing down; the ceiling is how a board says
/// "my signal integrity tops out here" without pretending to know the
/// source rate.
fn card_clock_ceiling(dt_max_hz: u32) -> u32 {
    let board_max_hz = u32::try_from(kbuild_config::MMC_MAX_CARD_HZ).unwrap_or(0);
    match (dt_max_hz, board_max_hz) {
        (0, board) => board,
        (dt, 0) => dt,
        (dt, board) => dt.min(board),
    }
}

/// Widest data bus this board drives (Linux `MMC_CAP_4_BIT_DATA`).
///
/// `MMC_4BIT=n` is the bring-up lever for a slot whose `DAT1..DAT3` do not
/// reach the controller: the card is then kept in 1-bit mode instead of
/// failing every data phase with a start-bit error.
fn bus_width_ceiling() -> BusWidth {
    if kbuild_config::MMC_4BIT {
        BusWidth::Bit4
    } else {
        BusWidth::Bit1
    }
}

/// Map host-construction failures onto the probe control flow.
fn host_error(err: MmcError) -> ProbeError {
    ProbeError::Fatal(match err {
        MmcError::Unsupported => RuntimeError::Unsupported,
        _ => RuntimeError::Invalid,
    })
}

driver_base::declare_driver!(PlatformBus, DwMmcRockchipDriver, "dwmmc-rockchip", IDS);
