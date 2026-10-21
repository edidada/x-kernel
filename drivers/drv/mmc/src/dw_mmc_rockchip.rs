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
    Bus, Device, DeviceDescriptor, DriverOps, IoMap, IrqAction, OfMatch, PlatformBus, PlatformId,
    ProbeError, RuntimeError,
};
use subsys_mmc::{
    DetectContext, MmcError,
    bus::{MmcBus, MmcCardData},
    sd,
};
use subsys_mmc_host::TaskXferSignal;

use super::dw_mmc::{DMA_WINDOW_BYTES, DwMmcHost, RING_PAGE};

/// `static const struct of_device_id dw_mmc_rockchip_match[]`.
static IDS: &[PlatformId] = &[
    PlatformId::Of(OfMatch::compatible("rockchip,rk3588-dw-mshc")),
    // The rk3588 node's secondary compatible — the generic dw_mshc
    // fallback Linux's of_device_id table matches the same way.
    PlatformId::Of(OfMatch::compatible("rockchip,rk3288-dw-mshc")),
];

/// Driver-private data (`struct dw_mci` ownership): the IRQ binding and
/// one reference to the shared host.
///
/// Drop order is the safety argument: this Data drops first at remove,
/// which unbinds the IRQ (`_irq`); the host itself — owning its DMA
/// mappings — lives until the LAST reference goes, which is the card
/// device's Data, cascaded away through the parent edge before this
/// device finishes teardown.
pub struct DwMmcRockchipData {
    /// Pins the host for the device's lifetime (see the drop-order note).
    _host: Arc<DwMmcHost>,
    /// Keeps the card-detect/data IRQ bound for the device's lifetime.
    _irq: driver_base::DevIrq,
}

/// The platform driver.
pub struct DwMmcRockchipDriver;

impl DriverOps<PlatformBus> for DwMmcRockchipDriver {
    type Data = DwMmcRockchipData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<DwMmcRockchipData, ProbeError> {
        let regs: IoMap = dev.res().iomap(0)?;
        let info = dev.info();
        let slot = u8::try_from(info.id().raw()).unwrap_or(0);
        let fifo_depth = dev.fw().u32("fifo-depth").unwrap_or(0x100);
        let max_frequency = dev.fw().u32("max-frequency").unwrap_or(50_000_000);

        // Clocks through the framework: biu gates the bus interface, ciu
        // is the card-clock input whose rate programs the divider. Enable
        // failures are tolerated with a warning — the firmware (U-Boot)
        // leaves these gates on, and some SCMI agents serve rate queries
        // without enable support; only a bogus rate is fatal.
        let _biu = dev.res().clk("biu")?;
        let ciu = dev.res().clk("ciu")?;
        if let Err(err) = ciu.prepare_enable() {
            log::warn!(
                "dwmmc-rocks: slot {slot}: ciu prepare_enable: {err:?} (continuing; \
                 firmware-configured clocks)"
            );
        }
        let ciu_hz = u32::try_from(ciu.get_rate()).map_err(|_| clock_error(()))?;
        if ciu_hz == 0 {
            return Err(ProbeError::Fatal(RuntimeError::Invalid));
        }
        log::info!(
            "dwmmc-rocks: slot {slot} at {:#x}, ciu {ciu_hz} Hz, fifo {fifo_depth}, max \
             {max_frequency} Hz",
            regs.region().base
        );

        // One coherent page of descriptors + the bounce window; the host
        // takes ownership of both (card consumers may outlive this Data).
        let ring = dev.res().alloc_coherent(RING_PAGE, RING_PAGE)?;
        let window = dev.res().alloc_coherent(DMA_WINDOW_BYTES, RING_PAGE)?;

        let host = Arc::new(
            DwMmcHost::new(
                regs,
                ciu_hz,
                fifo_depth,
                slot,
                ring,
                window,
                TaskXferSignal::shared(),
            )
            .map_err(host_error)?,
        );

        // Data-completion interrupt: the handler upgrades into the host
        // state and publishes transfer status. The Weak keeps removal
        // safe — after Data drops the handler safely declines.
        let weak = Arc::downgrade(&host);
        let spec = dev.res().irq(0)?;
        let irq = dev
            .res()
            .request_irq(spec, "dwmmc-rockchip", move || match weak.upgrade() {
                Some(host) if host.on_irq() => IrqAction::Handled,
                _ => IrqAction::NotHandled,
            })?;

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

        Ok(DwMmcRockchipData {
            _host: host,
            _irq: irq,
        })
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

/// Map kclk failures onto the probe control flow.
fn clock_error(_: impl core::fmt::Debug) -> ProbeError {
    ProbeError::Fatal(RuntimeError::Io)
}

/// Map host-construction failures onto the probe control flow.
fn host_error(err: MmcError) -> ProbeError {
    ProbeError::Fatal(match err {
        MmcError::Unsupported => RuntimeError::Unsupported,
        _ => RuntimeError::Invalid,
    })
}

driver_base::declare_driver!(PlatformBus, DwMmcRockchipDriver, "dwmmc-rockchip", IDS);
