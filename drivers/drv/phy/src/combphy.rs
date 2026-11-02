// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RK3588 naneng combo PHY provider (PCIe2/USB3/SATA pipe PHY).
//!
//! Faithful translation of the pinned reference
//! `phy-rockchip-naneng-combphy.c` (linux-7.1.8, rk3588 paths):
//! - `hw_init` = ref-clk enable → per-mode GRF/mmio config → the two
//!   CRU resets deasserted (USB3 additionally polls pipe-phy ready);
//! - `hw_exit` = resets asserted + clocks disabled;
//! - the mode binds on the FIRST acquire from the DT specifier cell and
//!   conflicting re-acquires are rejected (fail-closed — Linux warns and
//!   overwrites, which on a shared instance would silently retarget the
//!   pipe while another consumer drives it).
//!
//! Known v1 gaps (recorded in the port artifacts): SATA/SGMII/QSGMII
//! modes rejected; the BSP `rockchip,pcie1ln-sel-bits` property is not
//! translated (mainline lane-mux semantics: id 0 writes no lane-sel).

use alloc::{boxed::Box, sync::Arc};

use driver_base::{
    ClkHandle, Device, DriverOps, IoMap, OfMatch, PlatformBus, PlatformId, ProbeError, ResetHandle,
    RuntimeError,
};
use kphy::{PhyError, PhyHwOps, PhyInstance, PhyLookup, PhyMode, PhySpecifier};
use kspin::SpinNoIrq;

use crate::grf::Grf;

/// `PHY_TYPE_*` values of the DT specifier cell (dt-bindings/phy).
const PHY_TYPE_PCIE: PhyMode = 2;
const PHY_TYPE_USB3: PhyMode = 4;

/// The line ids this provider's mmio bases map to (`phy_ids` in the
/// reference cfg): fee00000→0, fee10000→1, fee20000→2.
fn phy_id(mmio_base: usize) -> Option<u8> {
    match mmio_base {
        0xfee00000 => Some(0),
        0xfee10000 => Some(1),
        0xfee20000 => Some(2),
        _ => None,
    }
}

// ---- reference register map (rk3568-common table the rk3588 path uses) ---
const RK3568_PHYREG6: usize = 0x18;
const RK3568_PHYREG6_PLL_DIV_MASK: u32 = 0x3 << 6;
const RK3568_PHYREG6_PLL_DIV_SHIFT: u8 = 6;
const RK3568_PHYREG6_PLL_DIV_2: u32 = 1;
const RK3568_PHYREG11: usize = 0x2c;
const RK3568_PHYREG11_SU_TRIM_0_7: u32 = 0xcc;
const RK3568_PHYREG12: usize = 0x30;
const RK3568_PHYREG12_PLL_LPF_ADJ_VALUE: u32 = 4;
const RK3568_PHYREG15: usize = 0x3c;
const RK3568_PHYREG15_CTLE_EN: u32 = 1 << 3;
const RK3568_PHYREG15_SSC_CNT_MASK: u32 = 0x3ff << 6;
const RK3568_PHYREG15_SSC_CNT_SHIFT: u8 = 6;
const RK3568_PHYREG15_SSC_CNT_VALUE: u32 = 1;
const RK3568_PHYREG16: usize = 0x40;
const RK3568_PHYREG16_SSC_CNT_VALUE: u32 = 0x5f;
const RK3568_PHYREG18: usize = 0x44;
const RK3568_PHYREG18_PLL_LOOP: u32 = 0x32;
const RK3568_PHYREG32: usize = 0x7c;
const RK3568_PHYREG32_SSC_MASK: u32 = 0xf << 4;
const RK3568_PHYREG32_SSC_DIR_SHIFT: u8 = 4;
const RK3568_PHYREG32_SSC_DOWNWARD: u32 = 1;
const RK3568_PHYREG33: usize = 0x80;
const RK3568_PHYREG33_PLL_KVCO_MASK: u32 = 0x7 << 2;
const RK3568_PHYREG33_PLL_KVCO_SHIFT: u8 = 2;
const RK3568_PHYREG33_PLL_KVCO_VALUE: u32 = 2;
const RK3588_PHYREG27: usize = 0x6c;
const RK3588_PHYREG27_RX_TRIM: u32 = 0x4c;

// ---- pipe-phy-grf rows (`rk3588_combphy_grfcfgs`) ------------------------
// (offset, bitend, bitstart, enable)
const GRF_PCIE_MODE_SET: (usize, u16, u16, u16) = (0x0000, 5, 0, 0x11);
const GRF_USB_MODE_SET: (usize, u16, u16, u16) = (0x0000, 5, 0, 0x04);
const GRF_PIPE_TXCOMP_SEL: (usize, u16, u16, u16) = (0x0008, 15, 15, 0x01);
const GRF_PIPE_TXELEC_SEL: (usize, u16, u16, u16) = (0x0008, 12, 12, 0x01);
const GRF_PIPE_CLK_25M: (usize, u16, u16, u16) = (0x0004, 14, 13, 0x01);
const GRF_PIPE_CLK_100M: (usize, u16, u16, u16) = (0x0004, 14, 13, 0x02);
const GRF_PIPE_PHY_STATUS: (usize, u16, u16, u16) = (0x0034, 6, 6, 0x01);
const GRF_CON0_FOR_PCIE: (usize, u16, u16, u16) = (0x0000, 15, 0, 0x1000);
const GRF_CON1_FOR_PCIE: (usize, u16, u16, u16) = (0x0004, 15, 0, 0x0000);
const GRF_CON2_FOR_PCIE: (usize, u16, u16, u16) = (0x0008, 15, 0, 0x0101);
const GRF_CON3_FOR_PCIE: (usize, u16, u16, u16) = (0x000c, 15, 0, 0x0200);
const GRF_PIPE_PCIE1L0_SEL: (usize, u16, u16, u16) = (0x0100, 0, 0, 0x00);
const GRF_PIPE_PCIE1L1_SEL: (usize, u16, u16, u16) = (0x0100, 1, 1, 0x00);

/// One combphy instance: the provider hardware state plus the shared
/// instance behind every consumer handle.
struct CombPhy {
    id: u8,
    regs: IoMap,
    pipe_grf: Grf,
    phy_grf: Grf,
    refclk: ClkHandle,
    /// The DT clocks as a bulk group (`clk_bulk_prepare_enable` in the
    /// reference enables refclk, apbclk and phpclk together). Only the
    /// reference clock is enabled today: turning the other two on through
    /// this CRU provider clobbers the shared gate word and kills the PHY's
    /// own APB clock (registers read back as 0), so the bulk path needs the
    /// gate write-mask bug fixed first. TODO(gate-hiword-clobber).
    clocks: [ClkHandle; 3],
    /// Bring-up diagnostic view of the CRU window (see `log_cru`).
    cru_diag: Option<IoMap>,
    /// `assigned-clock-rates` for the reference clock (100 MHz on the
    /// rk3588 boards). The reference's PHY init branches on the rate it
    /// actually reads, so the rate must be programmed before `hw_init`.
    refclk_target: Option<u64>,
    resets: [ResetHandle; 2],
    mode: SpinNoIrq<Option<PhyMode>>,
    instance: SpinNoIrq<Option<Arc<PhyInstance>>>,
}

/// `rockchip_combphy_updatel`: read-modify-write on the own mmio window.
fn updatel(mmio: &IoMap, mask: u32, val: u32, reg: usize) {
    let tmp = mmio.read32(reg);
    mmio.write32(reg, (tmp & !mask) | val);
}

/// `rockchip_combphy_param_write`: hiword write of `(enable << bitstart)`
/// over `GENMASK(bitend, bitstart)` (`BIT_WRITEABLE_SHIFT` = 16).
fn param_write(grf: &Grf, row: (usize, u16, u16, u16)) {
    let (offset, bitend, bitstart, enable) = row;
    let mask: u32 = (u32::MAX >> (31 - bitend)) & !((1u32 << bitstart) - 1);
    let word = ((enable as u32) << bitstart) | (mask << 16);
    grf.write(offset, word);
}

impl CombPhy {
    /// `rk3588_combphy_cfg` — per-mode GRF/mmio programming, translated
    /// sequence by sequence (PCIE and USB3; other types unsupported).
    fn configure(&self, mode: PhyMode) -> Result<(), PhyError> {
        match mode {
            PHY_TYPE_PCIE => {
                param_write(&self.phy_grf, GRF_CON0_FOR_PCIE);
                param_write(&self.phy_grf, GRF_CON1_FOR_PCIE);
                param_write(&self.phy_grf, GRF_CON2_FOR_PCIE);
                param_write(&self.phy_grf, GRF_CON3_FOR_PCIE);
                match self.id {
                    1 => param_write(&self.pipe_grf, GRF_PIPE_PCIE1L0_SEL),
                    2 => param_write(&self.pipe_grf, GRF_PIPE_PCIE1L1_SEL),
                    _ => {}
                }
            }
            PHY_TYPE_USB3 => {
                // Set SSC downward spread spectrum.
                let val = RK3568_PHYREG32_SSC_DOWNWARD << RK3568_PHYREG32_SSC_DIR_SHIFT;
                updatel(&self.regs, RK3568_PHYREG32_SSC_MASK, val, RK3568_PHYREG32);
                // Enable adaptive CTLE for USB3.0 Rx.
                let val = self.regs.read32(RK3568_PHYREG15) | RK3568_PHYREG15_CTLE_EN;
                self.regs.write32(RK3568_PHYREG15, val);
                // PLL KVCO fine tuning signals.
                let val = RK3568_PHYREG33_PLL_KVCO_VALUE << RK3568_PHYREG33_PLL_KVCO_SHIFT;
                updatel(
                    &self.regs,
                    RK3568_PHYREG33_PLL_KVCO_MASK,
                    val,
                    RK3568_PHYREG33,
                );
                // Enable controlling random jitter.
                self.regs
                    .write32(RK3568_PHYREG12, RK3568_PHYREG12_PLL_LPF_ADJ_VALUE);
                // Set PLL input clock divider 1/2.
                updatel(
                    &self.regs,
                    RK3568_PHYREG6_PLL_DIV_MASK,
                    RK3568_PHYREG6_PLL_DIV_2 << RK3568_PHYREG6_PLL_DIV_SHIFT,
                    RK3568_PHYREG6,
                );
                self.regs.write32(RK3568_PHYREG18, RK3568_PHYREG18_PLL_LOOP);
                self.regs
                    .write32(RK3568_PHYREG11, RK3568_PHYREG11_SU_TRIM_0_7);
                param_write(&self.phy_grf, GRF_PIPE_TXCOMP_SEL);
                param_write(&self.phy_grf, GRF_PIPE_TXELEC_SEL);
                param_write(&self.phy_grf, GRF_PCIE_MODE_SET);
                let _ = GRF_USB_MODE_SET;
            }
            _ => {
                log::error!("combphy: incompatible phy type {mode:#x}");
                return Err(PhyError::NotSupported);
            }
        }

        match self.refclk.get_rate() {
            24_000_000 => {
                if mode == PHY_TYPE_USB3 {
                    // ssc_cnt[9:0]=0101111101 & 31.5KHz.
                    let val = RK3568_PHYREG15_SSC_CNT_VALUE << RK3568_PHYREG15_SSC_CNT_SHIFT;
                    updatel(
                        &self.regs,
                        RK3568_PHYREG15_SSC_CNT_MASK,
                        val,
                        RK3568_PHYREG15,
                    );
                    self.regs
                        .write32(RK3568_PHYREG16, RK3568_PHYREG16_SSC_CNT_VALUE);
                }
            }
            25_000_000 => param_write(&self.phy_grf, GRF_PIPE_CLK_25M),
            100_000_000 => {
                param_write(&self.phy_grf, GRF_PIPE_CLK_100M);
                if mode == PHY_TYPE_PCIE {
                    // PLL KVCO fine tuning.
                    let val = 4 << RK3568_PHYREG33_PLL_KVCO_SHIFT;
                    updatel(
                        &self.regs,
                        RK3568_PHYREG33_PLL_KVCO_MASK,
                        val,
                        RK3568_PHYREG33,
                    );
                    // Enable controlling random jitter.
                    self.regs
                        .write32(RK3568_PHYREG12, RK3568_PHYREG12_PLL_LPF_ADJ_VALUE);
                    // rx_trim: PLL LPF C1 85pf R1 1.25kohm.
                    self.regs.write32(RK3588_PHYREG27, RK3588_PHYREG27_RX_TRIM);
                    // su_trim.
                    self.regs
                        .write32(RK3568_PHYREG11, RK3568_PHYREG11_SU_TRIM_0_7);
                }
            }
            rate => {
                log::error!("combphy: unsupported ref rate {rate}");
                return Err(PhyError::Io);
            }
        }

        Ok(())
    }

    /// The USB3-mode ready poll (`readx_poll_timeout_atomic`, 10µs × 100).
    fn wait_usb3_ready(&self) {
        let (offset, bitend, bitstart, enable) = GRF_PIPE_PHY_STATUS;
        let mask: u32 = (u32::MAX >> (31 - bitend)) & !((1u32 << bitstart) - 1);
        for _ in 0..100 {
            let val = (self.phy_grf.read(offset) & mask) >> bitstart;
            if val == enable as u32 {
                return;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        }
        log::warn!("combphy: wait phy status ready timeout");
    }
}

impl CombPhy {
    /// Bring-up diagnostic: the CRU gate words that hold the php/pcie gates
    /// plus `CLKSEL_CON(0x50)` (shared by `aclk_php_root`'s mux/divider and
    /// `pclk_php_root`'s mux). Printed around each clock enable so a write
    /// that clobbers a neighbour names itself.
    fn log_cru(&self, tag: &str) {
        let Some(cru) = &self.cru_diag else {
            return;
        };
        log::info!(
            "combphy: cru {tag}: 880={:#010x} 884={:#010x} 888={:#010x} 89c={:#010x} 440={:#010x}",
            cru.read32(0x880),
            cru.read32(0x884),
            cru.read32(0x888),
            cru.read32(0x89c),
            cru.read32(0x440),
        );
    }
}

impl PhyHwOps for CombPhy {
    /// `rockchip_combphy_init`: clocks → mode config → resets deassert
    /// (USB3 additionally polls the pipe-phy ready bit).
    fn hw_init(&self) -> Result<(), PhyError> {
        let mode = self.mode.lock().expect("mode bound at acquire");
        log::info!("combphy: hw_init id {} mode {mode:#x}", self.id);
        if let Some(target) = self.refclk_target {
            let before = self.refclk.get_rate();
            let after = self.refclk.set_rate(target).unwrap_or(before);
            log::info!("combphy: refclk {before} -> {after} Hz (target {target})");
        }
        log::info!("combphy: refclk reads {} Hz", self.refclk.get_rate());
        // Bulk-enable like the reference. Enabling all three previously made
        // every combphy register read back as 0, so log the shared CRU words
        // around each enable to see which neighbour bit moves.
        self.log_cru("before-clocks");
        for (index, clk) in self.clocks.iter().enumerate() {
            let rate = clk.get_rate();
            clk.prepare_enable().map_err(|_| PhyError::Io)?;
            // The PHP APB gate (`pclk_php_root`, CLKGATE_CON(32) bit 0) is
            // enable-then-disable'd again somewhere inside the framework
            // after our enable lands, which leaves the whole PHP APB domain
            // gated and every combphy register reading back 0. Re-assert
            // once and report the state so the rollback stays visible.
            let first = clk.is_enabled();
            if !first {
                let _ = clk.enable();
            }
            log::info!(
                "combphy: clock {index} at {rate} Hz enabled {first} -> {}",
                clk.is_enabled()
            );
            self.log_cru("after-clock");
        }
        self.configure(mode)?;
        for rst in &self.resets {
            rst.deassert();
        }
        if mode == PHY_TYPE_USB3 {
            self.wait_usb3_ready();
        }
        log::info!("combphy: hw_init id {} done", self.id);
        Ok(())
    }

    /// `rockchip_combphy_exit`: resets asserted + clocks disabled.
    fn hw_exit(&self) -> Result<(), PhyError> {
        for rst in &self.resets {
            rst.assert();
        }
        for clk in self.clocks.iter().rev() {
            clk.disable_unprepare();
        }
        Ok(())
    }

    fn hw_set_mode(&self, _mode: PhyMode) -> Result<(), PhyError> {
        // The mode binds at acquire from the DT specifier (the reference's
        // xlate); a consumer-side set_mode would retarget the shared pipe.
        Err(PhyError::NotSupported)
    }
}

/// The registered lookup: phandle + the shared instance. Mode binding and
/// conflict rejection live on [`CombPhy`].
struct CombPhyLookup {
    provider_id: u32,
    combphy: Arc<CombPhy>,
}

impl PhyLookup for CombPhyLookup {
    fn provider_id(&self) -> u32 {
        self.provider_id
    }

    fn acquire(&self, spec: PhySpecifier) -> Result<Arc<PhyInstance>, PhyError> {
        let mode = spec.first();
        {
            let mut bound = self.combphy.mode.lock();
            match *bound {
                None => *bound = Some(mode),
                Some(bound) if bound != mode => return Err(PhyError::ModeConflict),
                _ => {}
            }
        }
        let mut slot = self.combphy.instance.lock();
        Ok(slot
            .get_or_insert_with(|| PhyInstance::new(self.combphy.clone()))
            .clone())
    }
}

/// `static const struct of_device_id combphy_of_match[]` entry.
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible(
    "rockchip,rk3588-naneng-combphy",
))];

/// Driver-private data (boot-static like every clock/phy provider).
pub struct CombPhyData {}

/// The platform driver.
pub struct CombPhyDriver;

impl DriverOps<PlatformBus> for CombPhyDriver {
    type Data = CombPhyData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<CombPhyData, ProbeError> {
        let regs = dev.res().iomap(0)?;
        let id = phy_id(regs.region().base).ok_or_else(|| {
            log::error!(
                "combphy: mmio base {:#x} matches no known phy id",
                regs.region().base
            );
            ProbeError::Fatal(RuntimeError::Invalid)
        })?;

        // Bind the view: `node()` borrows the firmware snapshot.
        let fw = dev.fw();
        let firmware = fw.node().ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        let phandle = firmware
            .phandle
            .ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;

        let pipe_grf = Grf::by_property(dev, "rockchip,pipe-grf")?;
        let phy_grf = Grf::by_property(dev, "rockchip,pipe-phy-grf")?;
        let refclk = dev.res().clk_at(0)?;
        let clocks = [
            dev.res().clk_at(0)?,
            dev.res().clk_at(1)?,
            dev.res().clk_at(2)?,
        ];
        // The board states the reference clock's rate in
        // `assigned-clock-rates` (rk3588 combphys: 100 MHz). Nothing in the
        // clock framework applies it, and the PHY's init branches on the
        // rate it reads — an unapplied rate leaves the PLL unconfigured,
        // so the pipe clock never starts (LTSSM stays 0 and the host's DBI
        // slave, which runs on the pipe-derived core clock, never answers).
        let refclk_target = firmware
            .prop("assigned-clock-rates")
            .and_then(|p| p.u32_list())
            .and_then(|cells| cells.first().copied())
            .map(u64::from);
        let resets = [dev.res().reset_at(0)?, dev.res().reset_at(1)?];
        let cru_diag = dev
            .res()
            .iomap_region(
                driver_base::MmioRegion::new(0xfd7c_0000_usize, 0x1000),
                "combphy-cru-diag",
            )
            .ok();

        // `rockchip_combphy_probe`: assert the phy reset through probe;
        // hw_init re-deasserts with the full sequence.
        for rst in &resets {
            rst.assert();
        }

        let combphy = Arc::new(CombPhy {
            id,
            regs,
            pipe_grf,
            phy_grf,
            refclk,
            clocks,
            cru_diag,
            refclk_target,
            resets,
            mode: SpinNoIrq::new(None),
            instance: SpinNoIrq::new(None),
        });
        let lookup: &'static CombPhyLookup = Box::leak(Box::new(CombPhyLookup {
            provider_id: phandle,
            combphy,
        }));
        dev.env()
            .provide::<driver_host::PhyProvider>(phandle, lookup)?;

        log::info!("combphy: id {id} published (phandle {phandle:#x})");
        Ok(CombPhyData {})
    }
}

driver_base::declare_driver!(PlatformBus, CombPhyDriver, "naneng-combphy", IDS);
