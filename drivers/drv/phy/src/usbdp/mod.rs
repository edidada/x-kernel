// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RK3588 USBDP PHY provider (`rockchip,rk3588-usbdp-phy`), USB3 path only.
//!
//! Faithful translation of the pinned reference
//! `drivers/phy/rockchip/phy-rockchip-usbdp.c` (linux-7.1.8, byte-identical to
//! the linux-jr 6.1 BSP for this path), restricted to what the DWC3 host
//! consumes on this board:
//!
//! - probe reads the GRF syscon windows by phandle (`rockchip,usbdpphy-grf`,
//!   `rockchip,usb-grf`), the clocks, the five named resets, and the
//!   `rockchip,dp-lane-mux` mode, then publishes the `u3-port` as a kphy
//!   provider;
//! - `init` runs `udphy_init`: assert all resets, enable RX LFPS, power the
//!   PMA, deassert the APB resets, write the PMA init sequence + the refclk
//!   table, set the lane mux, deassert init/cmn/lane, and poll the LCPLL and
//!   RX-CDR lock status;
//! - `rockchip_u3phy_init` then clears the U3 port's GRF disable word
//!   (`usb3otg{0,1}_cfg`) before the controller touches the port.
//!
//! DisplayPort (`dp-port`, the VO GRF lane select, HPD, link training) is out
//! of the v1 surface: this board routes USB3 only. The `dp-lane-mux` property
//! is still honored for the lane-mux write and the `flip` flag, because a
//! USB+DP combo board uses it to decide which lane carries USB.

mod tables;

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use driver_base::{
    ClkHandle, Device, DriverOps, EnabledClock, IoMap, OfMatch, PlatformBus, PlatformId,
    ProbeError, ResetHandle, RuntimeError,
};
use kphy::{PhyError, PhyHwOps, PhyInstance, PhyLookup};
use tables::{INIT_SEQUENCE, REFCLK_24M_CFG};

/// PMA register window: the PHY node's `reg` window plus this offset.
const PMA_OFFSET: usize = 0x8000;

// PMA register offsets used by the init/lock path (reference `..._OFFSET`).
/// Lane mux + enable, one nibble-pair per lane.
const CMN_LANE_MUX_AND_EN: usize = 0x0288;
/// DP common/init reset.
const CMN_DP_RSTN: usize = 0x038c;
/// LCPLL lock status.
const CMN_ANA_LCPLL_DONE: usize = 0x0350;
/// Lane 0 / lane 2 RX CDR lock status.
const TRSV_LN0_MON_RX_CDR: usize = 0x0b84;
const TRSV_LN2_MON_RX_CDR: usize = 0x1b84;

const CMN_ANA_LCPLL_LOCK_DONE: u32 = 1 << 7;
const CMN_ANA_LCPLL_AFC_DONE: u32 = 1 << 6;
const TRSV_RX_CDR_LOCK_DONE: u32 = 1 << 0;

/// `udphy_parse_lane_mux_data`: how many `dp-lane-mux` cells mean what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No `dp-lane-mux` property: all four lanes carry USB.
    Usb,
    /// 1–2 lanes for DP, the rest USB.
    DpUsb,
    /// 4 lanes for DP, no USB.
    Dp,
}

/// A GRF word written with the Rockchip hiword convention (upper half = the
/// write-enable mask), `enable`/`disable` being the data value.
struct GrfReg {
    offset: usize,
    bitend: u32,
    bitstart: u32,
    disable: u32,
    enable: u32,
}

impl GrfReg {
    const fn new(offset: usize, bitend: u32, bitstart: u32, disable: u32, enable: u32) -> Self {
        Self {
            offset,
            bitend,
            bitstart,
            disable,
            enable,
        }
    }
}

fn grfreg_write(map: &IoMap, reg: &GrfReg, on: bool) {
    let tmp = if on { reg.enable } else { reg.disable };
    let mask = ((1u32 << (reg.bitend - reg.bitstart + 1)) - 1) << reg.bitstart;
    map.write32(reg.offset, ((tmp << reg.bitstart) & mask) | (mask << 16));
}

/// Per-controller GRF config (reference `rk3588_udphy_cfgs`).
struct UdphyCfg {
    /// usbdpphy-grf: PMA low-power negate.
    low_pwrn: GrfReg,
    /// usbdpphy-grf: RX LFPS enable (USB mode).
    rx_lfps: GrfReg,
    /// usb-grf: U3 port disable word for this controller's id.
    usb3otg_cfg: GrfReg,
}

fn cfg_of(id: usize) -> UdphyCfg {
    // Offsets/values verbatim from the reference `rk3588_udphy_cfgs.grfcfg`.
    // The 0x0188/0x1100 pair is the reference's, but its naming does not hold on
    // this board: `RK_UDPHY_GEN_GRF_REG(offset, bitend, bitstart, disable,
    // enable)` lists 0x1100 as *disable* and 0x0188 as *enable*, yet writing
    // 0x0188 makes the DWC3 report one host port while 0x1100 makes it report
    // two — measured by bracketing HCSPARAMS1 through the whole init sequence
    // (2 right up to the PHY bring-up, 1 afterwards, back to 2 with 0x1100).
    // Losing the second port means no SuperSpeed port at all, so the values are
    // swapped here relative to the reference's names, and the working board's
    // own usb-grf word is 0x1100 with SuperSpeed devices enumerated.
    let usb3otg_cfg = if id == 0 {
        GrfReg::new(0x001c, 15, 0, 0x0188, 0x1100)
    } else {
        GrfReg::new(0x0034, 15, 0, 0x0188, 0x1100)
    };
    UdphyCfg {
        low_pwrn: GrfReg::new(0x0004, 13, 13, 0, 1),
        rx_lfps: GrfReg::new(0x0004, 14, 14, 0, 1),
        usb3otg_cfg,
    }
}

/// One USBDP PHY instance (the shared hardware state behind the published
/// `u3-port`).
struct UsbDp {
    pma: IoMap,
    usbdp_grf: IoMap,
    usb_grf: IoMap,
    resets: Vec<(reset_names::Name, ResetHandle)>,
    /// The DT clocks (refclk / immortal / pclk / utmi). Handles only; the
    /// enable tokens live in `enabled_clocks` because dropping an
    /// [`EnabledClock`] gates the clock again.
    clocks: Vec<ClkHandle>,
    /// The enable tokens, held for as long as the PHY is initialised.
    /// `prepare_enable()` returns a guard whose `Drop` runs
    /// `disable`+`unprepare`; letting it fall out of scope right after
    /// `?` is the "enabled then immediately disabled" trap this tree hit on
    /// the usb2 phy (and the combphy) before.
    enabled_clocks: ksync::Mutex<Vec<EnabledClock>>,
    cfg: UdphyCfg,
    mode: Mode,
    lane_mux_sel: [u32; 4],
    flip: bool,
}

/// Reset names as the node lists them (`reset-names = "init","cmn","lane",
/// "pcs_apb","pma_apb"`); the index into the node's `resets` is fixed by DT.
mod reset_names {
    pub(crate) type Name = &'static str;
    pub(crate) const INIT: &str = "init";
    pub(crate) const CMN: &str = "cmn";
    pub(crate) const LANE: &str = "lane";
    pub(crate) const PCS_APB: &str = "pcs_apb";
    pub(crate) const PMA_APB: &str = "pma_apb";
}

impl UsbDp {
    fn reset(&self, name: &'static str) -> Option<&ResetHandle> {
        self.resets
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, handle)| handle)
    }

    fn assert_all(&self) {
        for (_, handle) in &self.resets {
            handle.assert();
        }
    }

    fn deassert(&self, name: &'static str) {
        if let Some(handle) = self.reset(name) {
            handle.deassert();
        }
    }

    fn write_pma(&self, offset: usize, value: u32) {
        self.pma.write32(offset, value);
    }

    fn update_pma(&self, offset: usize, mask: u32, value: u32) {
        let current = self.pma.read32(offset);
        self.pma.write32(offset, (current & !mask) | (value & mask));
    }

    /// `udphy_refclk_set` for the board's 24 MHz reference.
    fn refclk_set(&self) -> Result<(), PhyError> {
        for row in REFCLK_24M_CFG {
            self.write_pma(row.offset, row.value);
        }
        Ok(())
    }

    /// `udphy_init` (USB mode). The full analog/PLL bring-up.
    fn init(&self) -> Result<(), PhyError> {
        // Assert everything, then settle before touching the PMA.
        self.assert_all();
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10_000));

        if matches!(self.mode, Mode::Usb | Mode::DpUsb) {
            grfreg_write(&self.usbdp_grf, &self.cfg.rx_lfps, true);
        }

        // Step 1: power the PMA and deassert the APB resets.
        grfreg_write(&self.usbdp_grf, &self.cfg.low_pwrn, true);
        self.deassert(reset_names::PMA_APB);
        self.deassert(reset_names::PCS_APB);

        // Step 2: the init sequence, then the refclk table.
        for row in INIT_SEQUENCE {
            self.write_pma(row.offset, row.value);
        }
        self.refclk_set()?;

        // Step 3: lane mux; all DP-lane enables stay cleared (USB owns them).
        let mut mux = 0u32;
        for (lane, sel) in self.lane_mux_sel.iter().enumerate() {
            mux |= sel << (lane + 4);
        }
        self.update_pma(CMN_LANE_MUX_AND_EN, 0xff, mux);

        // Step 4: deassert init reset (USB) / DP init reset, ~200 ns settle.
        if matches!(self.mode, Mode::Usb | Mode::DpUsb) {
            self.deassert(reset_names::INIT);
        }
        if matches!(self.mode, Mode::Dp | Mode::DpUsb) {
            self.update_pma(CMN_DP_RSTN, 1 << 3, 1 << 3);
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1));

        // Step 5: deassert the common/lane resets (USB).
        if matches!(self.mode, Mode::Usb | Mode::DpUsb) {
            self.deassert(reset_names::CMN);
            self.deassert(reset_names::LANE);
        }

        // Step 6: wait for the LCPLL and RX-CDR locks.
        self.status_check()
    }

    /// `udphy_status_check`: poll LCPLL lock, then the RX-CDR lock on the lane
    /// that carries USB (`lane 0` normally, `lane 2` when the mux is flipped).
    fn status_check(&self) -> Result<(), PhyError> {
        if matches!(self.mode, Mode::Usb | Mode::DpUsb) {
            self.poll_pma(
                CMN_ANA_LCPLL_DONE,
                CMN_ANA_LCPLL_AFC_DONE | CMN_ANA_LCPLL_LOCK_DONE,
                "lcpll",
            )?;
            let (offset, name) = if self.flip {
                (TRSV_LN2_MON_RX_CDR, "ln2 rx cdr")
            } else {
                (TRSV_LN0_MON_RX_CDR, "ln0 rx cdr")
            };
            self.poll_pma(offset, TRSV_RX_CDR_LOCK_DONE, name)?;
        }
        Ok(())
    }

    fn poll_pma(&self, offset: usize, mask: u32, what: &str) -> Result<(), PhyError> {
        for _ in 0..500 {
            if self.pma.read32(offset) & mask == mask {
                return Ok(());
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(200));
        }
        log::error!(
            "usbdp: {what} lock timeout (pma {offset:#x} = {:#010x})",
            self.pma.read32(offset)
        );
        Err(PhyError::Io)
    }

    /// `udphy_u3_port_disable`: clear (enable) or set the U3 port's GRF word.
    fn u3_port_disable(&self, disable: bool) {
        // The word this builds is chosen by the DWC3's port count, not by the
        // reference's field names (see `cfg_of`): the value the reference calls
        // "disable" is the one that makes the controller expose its SuperSpeed
        // port, and the value it calls "enable" removes it.
        grfreg_write(&self.usb_grf, &self.cfg.usb3otg_cfg, !disable);
        log::info!(
            "usbdp: usb3otg cfg (disable {disable}) reads {:#06x}",
            self.usb_grf.read32(self.cfg.usb3otg_cfg.offset) & 0xffff,
        );
    }
}

impl PhyHwOps for UsbDp {
    /// `rockchip_u3phy_init`: enable the clocks, the U3 port, then power on
    /// USB mode. The clocks are enabled here (not held from probe) so the
    /// consumer's acquire edge is what powers the PHY, matching the reference
    /// `udphy_setup`'s `clk_bulk_prepare_enable`.
    fn hw_init(&self) -> Result<(), PhyError> {
        {
            let mut tokens = self.enabled_clocks.lock();
            if tokens.is_empty() {
                for (index, clock) in self.clocks.iter().enumerate() {
                    // Keep the token: dropping it gates the clock again.
                    let token = clock.prepare_enable().map_err(|err| {
                        log::error!("usbdp: clock {index} enable failed: {err:?}");
                        PhyError::Io
                    })?;
                    log::info!(
                        "usbdp: clock {index} enabled ({} Hz) and held",
                        clock.get_rate()
                    );
                    tokens.push(token);
                }
            }
        }
        if matches!(self.mode, Mode::Dp) {
            // DP-only (or high-speed-only) routing: keep the U3 port off.
            self.u3_port_disable(true);
            return Ok(());
        }
        self.u3_port_disable(false);
        self.init()
    }

    /// `rockchip_u3phy_exit`: power USB mode back off.
    fn hw_exit(&self) -> Result<(), PhyError> {
        self.assert_all();
        self.u3_port_disable(true);
        // Release the clock tokens (drops => disable + unprepare).
        self.enabled_clocks.lock().clear();
        Ok(())
    }

    fn hw_set_mode(&self, _mode: kphy::PhyMode) -> Result<(), PhyError> {
        Err(PhyError::NotSupported)
    }
}

/// The registered lookup: the `u3-port` phandle + the shared instance.
struct UsbDpLookup {
    provider_id: u32,
    instance: Arc<PhyInstance>,
}

impl PhyLookup for UsbDpLookup {
    fn provider_id(&self) -> u32 {
        self.provider_id
    }

    fn acquire(&self, _spec: kphy::PhySpecifier) -> Result<Arc<PhyInstance>, PhyError> {
        // `#phy-cells = <0>` on the u3-port: no specifier state to bind.
        Ok(self.instance.clone())
    }
}

/// `of_device_id usbdp_phy_id_table[]` entry.
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible(
    "rockchip,rk3588-usbdp-phy",
))];

/// Driver-private data (boot-static provider).
pub struct UsbDpData {}

/// The platform driver.
pub struct UsbDpDriver;

impl UsbDpDriver {
    /// Resolve a phandle-valued property to that node's mapped GRF window.
    fn map_syscon(
        dev: &Device<'_, PlatformBus>,
        prop: &str,
        name: &'static str,
    ) -> Result<IoMap, ProbeError> {
        let phandle = dev
            .fw()
            .u32(prop)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        let node =
            of::find_node_by_phandle(phandle).ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        let base = of::node_reg(node).ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        // GRF syscon windows are 0x4000 on this SoC generation.
        dev.res()
            .iomap_region(driver_base::MmioRegion::new(base as usize, 0x4000), name)
    }

    /// The `u3-port` child phandle: consumers reference it in their `phys`.
    /// `FdtNode` has no child walk, so scan all nodes for the one whose
    /// parent carries this controller's phandle and whose name is `u3-port`.
    fn u3_port_phandle(phandle: u32) -> Result<u32, ProbeError> {
        let fdt = of::fdt().ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        for node in fdt.all_nodes() {
            if node.name == "u3-port" && node.parent_property_u32("phandle") == Some(phandle) {
                return node
                    .property_u32("phandle")
                    .ok_or(ProbeError::Fatal(RuntimeError::Invalid));
            }
        }
        log::error!("usbdp: {phandle:#x} has no u3-port child");
        Err(ProbeError::Fatal(RuntimeError::NotFound))
    }
}

impl DriverOps<PlatformBus> for UsbDpDriver {
    type Data = UsbDpData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<UsbDpData, ProbeError> {
        let fw = dev.fw();
        let firmware = fw.node().ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        let phandle = firmware
            .phandle
            .ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        log::info!("usbdp: probing (phandle {phandle:#x})");

        // The PHY's own `reg` is the controller window; the PMA block starts
        // at +0x8000 inside it.
        let base = dev.res().mmio(0).map_err(|err| {
            log::error!("usbdp: {phandle:#x} has no mmio: {err:?}");
            err
        })?;
        let pma = dev
            .res()
            .iomap_region(
                driver_base::MmioRegion::new(base.base + PMA_OFFSET, base.size - PMA_OFFSET),
                "usbdp-pma",
            )
            .map_err(|err| {
                log::error!(
                    "usbdp: {phandle:#x} PMA map at {:#x}+{:#x} failed: {err:?}",
                    base.base + PMA_OFFSET,
                    base.size - PMA_OFFSET,
                );
                err
            })?;

        let usbdp_grf = Self::map_syscon(dev, "rockchip,usbdpphy-grf", "usbdpphy-grf")?;
        let usb_grf = Self::map_syscon(dev, "rockchip,usb-grf", "usb-grf")?;

        // Clocks: refclk / immortal / pclk / utmi. Handles only here — the
        // enable runs in `hw_init` on the consumer's acquire edge (the
        // reference enables them in `udphy_setup`, after the consumer binds).
        //
        // A clock that is not published yet is a real dependency: the `utmi`
        // entry is the usb2phy block's `usb480m_phyN` output, which a sibling
        // provider publishes, and running without it leaves the PHY without
        // its 480 MHz reference. Hand the deferral back (Linux
        // -EPROBE_DEFER) instead of skipping the clock.
        let mut clocks = Vec::new();
        for index in 0..4 {
            match dev.res().clk_at(index) {
                Ok(clock) => clocks.push(clock),
                Err(err @ ProbeError::Deferred(_)) => {
                    log::info!("usbdp: clock {index} not published yet, deferring ({err:?})");
                    return Err(err);
                }
                Err(err) => log::warn!("usbdp: clock {index} unavailable: {err:?}"),
            }
        }

        // The five named resets, in DT order.
        let names = [
            reset_names::INIT,
            reset_names::CMN,
            reset_names::LANE,
            reset_names::PCS_APB,
            reset_names::PMA_APB,
        ];
        let mut resets = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let handle = dev.res().reset_at(index).map_err(|err| {
                log::error!("usbdp: {phandle:#x} reset {index} ({name}) unavailable: {err:?}");
                err
            })?;
            resets.push((*name, handle));
        }

        // `rockchip,dp-lane-mux`: absent = USB on all lanes.
        let (mode, lane_mux_sel, flip) = parse_lane_mux(dev);
        let id = if base.base == 0xfed8_0000 { 0 } else { 1 };

        let phy = UsbDp {
            pma,
            usbdp_grf,
            usb_grf,
            resets,
            clocks,
            enabled_clocks: ksync::Mutex::new(Vec::new()),
            cfg: cfg_of(id),
            mode,
            lane_mux_sel,
            flip,
        };
        log::info!(
            "usbdp: phy {id} ({:#x}) mode {mode:?} flip {} lane_mux {:?}",
            base.base,
            phy.flip,
            phy.lane_mux_sel,
        );

        let instance = PhyInstance::new(Arc::new(phy));
        let port_phandle = Self::u3_port_phandle(phandle)?;
        log::info!("usbdp: phy {id} u3-port phandle {port_phandle:#x}");
        let lookup: &'static UsbDpLookup = Box::leak(Box::new(UsbDpLookup {
            provider_id: port_phandle,
            instance,
        }));
        dev.env()
            .provide::<driver_host::PhyProvider>(port_phandle, lookup)?;
        log::info!("usbdp: u3-port published (port phandle {port_phandle:#x})");

        Ok(UsbDpData {})
    }
}

/// `udphy_parse_lane_mux_data`: read `rockchip,dp-lane-mux` and derive the
/// mode, the per-lane mux nibble values, and the USB lane flip flag.
fn parse_lane_mux(dev: &Device<'_, PlatformBus>) -> (Mode, [u32; 4], bool) {
    let mut lane_mux_sel = [0u32; 4];
    let cells = dev
        .fw()
        .prop("rockchip,dp-lane-mux")
        .and_then(|prop| match prop {
            driver_base::FirmwareProp::U32(value) => Some(alloc::vec![value]),
            other => other.u32_list(),
        });

    let Some(lanes) = cells else {
        return (Mode::Usb, lane_mux_sel, false);
    };
    let num = lanes.len();
    if num != 1 && num != 2 && num != 4 {
        log::error!("usbdp: invalid dp-lane-mux lane count {num}");
        return (Mode::Usb, lane_mux_sel, false);
    }
    for (i, &lane) in lanes.iter().enumerate() {
        if lane > 3 {
            log::error!("usbdp: dp-lane-mux lane {lane} out of range");
            continue;
        }
        lane_mux_sel[lane as usize] = 1; // PHY_LANE_MUX_DP
        let _ = i;
    }
    let mode = if num == 4 { Mode::Dp } else { Mode::DpUsb };
    // USB is carried on lane 0 unless the mux moved DP onto it.
    let flip = lane_mux_sel[0] == 1;
    (mode, lane_mux_sel, flip)
}

driver_base::declare_driver!(PlatformBus, UsbDpDriver, "usbdp", IDS);
