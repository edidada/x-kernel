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
    Bus, Device, DeviceDescriptor, DriverOps, IoMap, IrqAction, MmioRegion, OfMatch, PlatformBus,
    PlatformId, ProbeError, RuntimeError,
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

        // Clocking, the board-verified tgoskits/U-Boot shape: the CCLK_SD
        // source and HCLK_SD gate sit behind the Rockchip bl31 blob, whose
        // SMC entry does not serve the mainline SCMI clock protocol
        // (probing it costs three round trips and a HARDWARE_ERROR; see
        // drv/clk/scmi_clock for the full investigation). The boot stages
        // leave the SD clock domain configured, so the driver does NOT
        // reprogram the source — instead it READS the live rate back from
        // the Secure CRU CLKSEL_CON registers (same math as U-Boot SPL's
        // rk3588_scru_clk_get_rate), falling back to the board fact
        // MMC_CIU_HZ when the SCRU reads as inaccessible/zero.
        let ciu_hz = probe_scru_cclk_hz().unwrap_or_else(|| {
            let fallback = kbuild_config::MMC_CIU_HZ as u32;
            log::warn!(
                "dwmmc-rocks: SCRU rate probe unavailable; using MMC_CIU_HZ={fallback:#x} Hz"
            );
            fallback
        });
        if ciu_hz == 0 {
            log::warn!(
                "dwmmc-rocks: ciu rate resolved to 0; slot {slot} cannot program its divider"
            );
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

/// SCRU (Secure CRU) base — the SD clock domain's mux/div live here.
const SCRU_PADDR: usize = 0xfd7d_0000;
/// `CLKSEL_CON(n)` stride inside the SCRU (U-Boot `RK3588_CLKSEL_CON`).
const fn scru_clksel_con(n: usize) -> usize {
    0x300 + n * 4
}
const OSC_HZ: u64 = 24_000_000;
const GPLL_HZ: u64 = 1_188_000_000;
const SPLL_HZ: u64 = 702_000_000;

/// Read the live CCLK_SD rate from the SCRU (U-Boot SPL
/// `rk3588_scru_clk_get_rate` math): CLKSEL_CON(3) holds the parent
/// select [13:12] (GPLL/SPLL/24M) and the divider [11:6]
/// (rate = parent / (div + 1)). Returns `None` when the registers read
/// as inaccessible (secure-only fault behavior reads-as-zero) — the
/// caller falls back to the Kconfig board fact.
fn probe_scru_cclk_hz() -> Option<u32> {
    // SAFETY: the SCRU aperture is a 4 KiB device window at a fixed SoC
    // address; reads are the same volatile MMIO the clock hardware
    // defines. On systems where the window is secure-only the read
    // returns garbage/zero, which the `?`-guards below reject.
    let scru = unsafe { IoMap::new(SCRU_PADDR, MmioRegion::new(SCRU_PADDR, 0x1000), None) };
    let con = scru.read32(scru_clksel_con(3));
    let sel = (con >> 12) & 0x3;
    let div = (con >> 6) & 0x3F;
    if div == 0 {
        return None;
    }
    let parent = match sel {
        0 => GPLL_HZ,
        1 => SPLL_HZ,
        2 => OSC_HZ,
        _ => return None,
    };
    if parent == 0 {
        return None;
    }
    // Sanity: the divider must be consistent with a real clock tree —
    // reads-as-zero windows yield div == 0 and are rejected above; a
    // nonsense rate (> parent) is impossible by construction.
    let rate = parent / (u64::from(div) + 1);
    log::info!("dwmmc-rocks: SCRU CCLK_SD sel {sel} div {div} -> {rate} Hz");
    u32::try_from(rate).ok()
}

/// Map host-construction failures onto the probe control flow.
fn host_error(err: MmcError) -> ProbeError {
    ProbeError::Fatal(match err {
        MmcError::Unsupported => RuntimeError::Unsupported,
        _ => RuntimeError::Invalid,
    })
}

driver_base::declare_driver!(PlatformBus, DwMmcRockchipDriver, "dwmmc-rockchip", IDS);
