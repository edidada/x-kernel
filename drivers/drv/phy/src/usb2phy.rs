// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RK3588 USB2 PHY provider (inno-usb2, HOST-path subset).
//!
//! Faithful translation of the pinned reference `phy-rockchip-inno-usb2.c`
//! (linux-7.1.8) for the board's consumers:
//! - probe runs `rk3588_usb2phy_tuning` (SIDDQ deassert, the phy reset
//!   pulse, suspend configuration, HS trim/pre-emphasis — block-relative
//!   GRF writes; the block comes from the node's own `reg` offset inside
//!   its usb2phy-grf parent) and the CRU reset pair is acquired;
//! - `power_on`/`power_off` = the `phy_sus` suspend bit plus the 480m
//!   clock gate (the `clkout` CRU gate is exposed to the consumer as an
//!   ordinary clock);
//! - `init` for HOST mode in Linux only arms the linestate/disconnect
//!   IRQs — interrupt-driven disconnect detection is NOT in the v1
//!   surface (consumers poll port status), recorded as a gap.
//!
//! OTG/BC11/VDP paths and the `rockchip,usbctrl-grf` U3 port enable are
//! out of the v1 surface (no Type-C/OTG consumer yet).

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};

use driver_base::{
    ClkHandle, Device, DriverOps, EnabledClock, IoMap, OfMatch, PlatformBus, PlatformId,
    ProbeError, ResetHandle, RuntimeError,
};
use kclk::{ClkBuilder, ClkCore, ClkHwOps, ClkLookup, ClkResult, ProviderId, Rate};
use kphy::{PhyError, PhyHwOps, PhyInstance, PhyLookup};

/// The usb2phy-grf block this port node belongs to (the node's own `reg`
/// base: 0x0000/0x4000 = USB3 OTG pairs, 0x8000/0xc000 = USB2 host pairs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Usb3_0,
    Usb3_1,
    Usb2_0,
    Usb2_1,
}

fn block_of(reg_base: u64) -> Option<Block> {
    match reg_base {
        0x0000 => Some(Block::Usb3_0),
        0x4000 => Some(Block::Usb3_1),
        0x8000 => Some(Block::Usb2_0),
        0xc000 => Some(Block::Usb2_1),
        _ => None,
    }
}

/// One usb2phy port: the mapped usb2phy-grf window, the port block, the
/// 480m clock and the CRU reset pair.
struct Usb2Phy {
    grf: IoMap,
    block: Block,
    clk480m: ClkHandle,
    /// The enable token has to be *held*: dropping it gates the clock again.
    /// Without the 480 MHz reference the PHY still detects a connect (that path
    /// runs off the always-on UTMI clock) but no transaction can ever complete,
    /// which is exactly how a controller that never answers SET_ADDRESS looks.
    clk480m_enabled: ksync::Mutex<Option<EnabledClock>>,
    resets: [ResetHandle; 2],
    /// Linux drives power through the `phy_sus` bit — SUSPEND asserted
    /// means powered DOWN (note the inverted polarity vs reset lines).
    phy_sus_offset: usize,
    phy_sus_bit: u8,
}

impl Usb2Phy {
    /// `rockchip_usb2phy_reset`: assert both CRU resets, 10µs, deassert,
    /// 100–200µs (busy waits — probe context).
    fn reset_pulse(&self) {
        for rst in &self.resets {
            rst.assert();
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        for rst in &self.resets {
            rst.deassert();
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(150));
    }

    /// `rk3588_usb2phy_tuning` — the block-relative GRF sequence. Only
    /// the block differs; every write below is verbatim from the
    /// reference (plain packed values — the inno rk3588 GRF bank is NOT
    /// hiword-conventioned).
    fn tuning(&self) {
        // utmi_termselect = 1'b1 (FS terminations),
        // utmi_xcvrselect = 2'b01 (FS transceiver);
        // USB3 blocks add utmi_opmode = 2'b01 (no-driving).
        let suspend_cfg: u32 = match self.block {
            Block::Usb3_0 | Block::Usb3_1 => 0x14 | 0x01,
            Block::Usb2_0 | Block::Usb2_1 => 0x14,
        };

        // Deassert SIDDQ to power on the analog block: GENMASK(29,29).
        self.grf.write32(0x0008, 1 << 29);
        // Do reset after exit IDDQ mode.
        self.reset_pulse();
        // Suspend configuration: GENMASK(20,16) | suspend_cfg.
        self.grf.write32(0x000c, (0x1f << 16) | suspend_cfg);
        // HS DC Voltage Level Adjustment 4'b1001: +5.89%.
        self.grf.write32(0x0004, (0xf << 24) | 0x0900);
        // HS Transmitter Pre-Emphasis Current Control 2'b10: 2x.
        self.grf.write32(0x0008, (0x3 << 19) | 0x0010);
        // The USB3_0/USB3_1 blocks get one more step in the reference: pull up the
        // IDDIG pin so the port behaves as a host. Without it the OTG port never
        // leaves its default role, which is consistent with a port that detects a
        // connect but cannot complete a transaction.
        if matches!(self.block, Block::Usb3_0 | Block::Usb3_1) {
            self.grf.write32(0x0010, (0x3 << 16) | 0x0003);
        }
    }

    /// `phy_sus` suspend-bit write (hiword convention): `false` = power
    /// on, `true` = suspend.
    fn set_suspended(&self, suspended: bool) {
        let mask = 1u32 << self.phy_sus_bit;
        let tmp: u32 = if suspended { 1 } else { 0 };
        self.grf.write32(
            self.phy_sus_offset,
            (tmp << self.phy_sus_bit) | (mask << 16),
        );
    }
}

impl PhyHwOps for Usb2Phy {
    /// `rockchip_usb2phy_init` (HOST/OTG host subset): clear and enable the
    /// linestate and VBUS-valid detect circuits. The reference also arms the
    /// detect IRQs, which is out of the v1 surface (consumers poll port
    /// status), but the detection enable bits themselves decide whether the
    /// `ls_det`/`bvalid` status words ever report anything.
    fn hw_init(&self) -> Result<(), PhyError> {
        match self.block {
            Block::Usb3_0 | Block::Usb3_1 => {
                // OTG port: clear bvalid/ls detect, then enable both.
                self.grf.write32(0x0088, 0x3 | (0x3 << 16));
                self.grf.write32(0x0080, 0x3 | (0x3 << 16));
            }
            Block::Usb2_0 | Block::Usb2_1 => {
                // Host port: clear ls detect, then enable it.
                self.grf.write32(0x0088, 0x1 | (0x1 << 16));
                self.grf.write32(0x0080, 0x1 | (0x1 << 16));
            }
        }
        Ok(())
    }

    /// `rockchip_usb2phy_exit`: nothing beyond the power pair for HOST.
    fn hw_exit(&self) -> Result<(), PhyError> {
        Ok(())
    }

    /// `rockchip_usb2phy_power_on`: `phy_sus` cleared, then a fresh reset and
    /// the UTMI stabilisation wait (the reference resets again when leaving
    /// suspend because the analog blocks were powered down). The 480 MHz
    /// output itself is opened by the consumer's `usb480m_phyN` prepare —
    /// matching Linux, where `clkout_ctl` is lazy and reads 0 even on the
    /// working board, so it is NOT driven from here.
    fn hw_power_on(&self) -> Result<(), PhyError> {
        let enabled = self.clk480m.prepare_enable().map_err(|_| PhyError::Io)?;
        *self.clk480m_enabled.lock() = Some(enabled);
        self.set_suspended(false);
        self.reset_pulse();
        // The reference (and the U-Boot code it mirrors) waits 2 ms here for the
        // UTMI clock to stabilise before the controller is allowed to touch the
        // port; without it the PHY can accept a connect but not a transaction.
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(2_000));
        Ok(())
    }

    /// `rockchip_usb2phy_power_off`: `phy_sus` set + clock gated.
    fn hw_power_off(&self) {
        self.set_suspended(true);
        // Dropping the token releases the same reference that power_on took.
        *self.clk480m_enabled.lock() = None;
    }

    fn hw_set_mode(&self, _mode: kphy::PhyMode) -> Result<(), PhyError> {
        Err(PhyError::NotSupported)
    }
}

/// The registered lookup: phandle + the shared instance.
struct Usb2PhyLookup {
    provider_id: u32,
    instance: Arc<PhyInstance>,
}

impl PhyLookup for Usb2PhyLookup {
    fn provider_id(&self) -> u32 {
        self.provider_id
    }

    fn acquire(&self, _spec: kphy::PhySpecifier) -> Result<Arc<PhyInstance>, PhyError> {
        // `#phy-cells = <0>`: no specifier state to bind.
        Ok(self.instance.clone())
    }
}

/// `static const struct of_device_id usb2phy_of_match[]` entry.
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible(
    "rockchip,rk3588-usb2phy",
))];

/// The PHY's 480 MHz output as a clock provider.
///
/// The firmware gives the phy node `#clock-cells = <0>` and
/// `clock-output-names = "usb480m_phyN"`, and both EHCI (`utmi`) and the DWC3
/// reference it as a clock source. This provider published only the PHY
/// instance, so those `clocks` entries resolved to nothing and the controllers
/// ran without their 480 MHz reference -- with connect detection working and no
/// transaction ever completing.
struct Usb2PhyClockLookup {
    provider_id: ProviderId,
    name: &'static str,
    core: Arc<ClkCore>,
}

/// `clkout_ctl`: the GRF bit that gates the PHY's 480 MHz output.
///
/// The reference (`rk3588_phy_cfgs`) puts it at block-relative offset 0x0000,
/// bit 0, with 1 = clock out / 0 = off; it is written with the inno GRF
/// hiword convention (upper half = write-enable mask). Linux opens it lazily
/// from `usb480m_phyN`'s `prepare` and waits ~1.2 ms for the clock to
/// stabilise; our previous `FixedRate` publication never touched the bit, so
/// the controllers ran without their 480 MHz reference even though connect
/// detection (an always-on path) still worked.
struct Usb2Phy480mOps {
    grf: IoMap,
    enabled: AtomicBool,
}

impl Usb2Phy480mOps {
    const BIT: u32 = 0;
    const OFFSET: usize = 0x0000;

    fn write_ctl(&self, on: bool) {
        let mask = 1u32 << Self::BIT;
        let value: u32 = if on { 1 } else { 0 };
        self.grf
            .write32(Self::OFFSET, (value << Self::BIT) | (mask << 16));
    }
}

impl ClkHwOps for Usb2Phy480mOps {
    /// `rockchip_usb2phy_clk480m_prepare`: open the output gate, then wait
    /// for the clock to stabilise before the consumer relies on it.
    fn prepare(&self) -> ClkResult {
        if !self.enabled.load(Ordering::Acquire) {
            self.write_ctl(true);
            self.enabled.store(true, Ordering::Release);
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1_250));
        }
        Ok(())
    }

    /// `rockchip_usb2phy_clk480m_unprepare`.
    fn unprepare(&self) -> ClkResult {
        self.write_ctl(false);
        self.enabled.store(false, Ordering::Release);
        Ok(())
    }

    /// `rockchip_usb2phy_clk480m_prepared`.
    fn is_prepared(&self) -> ClkResult<bool> {
        Ok(self.enabled.load(Ordering::Acquire))
    }

    /// `rockchip_usb2phy_clk480m_recalc_rate`: the output is a fixed 480 MHz
    /// divided from the `phyclk` reference; the divider is not programmable.
    fn recalc_rate(&self, _parent_rate: Rate) -> ClkResult<Rate> {
        Ok(480_000_000)
    }
}

impl ClkLookup for Usb2PhyClockLookup {
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

/// Driver-private data (boot-static provider).
pub struct Usb2PhyData {}

/// The platform driver.
pub struct Usb2PhyDriver;

impl DriverOps<PlatformBus> for Usb2PhyDriver {
    type Data = Usb2PhyData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<Usb2PhyData, ProbeError> {
        // Bind the view: `node()` borrows the firmware snapshot.
        let fw = dev.fw();
        let firmware = fw.node().ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        let phandle = firmware
            .phandle
            .ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;

        // The node's own `reg` is an OFFSET into the usb2phy-grf parent
        // window (the parent property block rides along on the node).
        let node: of::FdtNode<'static, 'static> =
            of::find_node_by_phandle(phandle).ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        let block_base = of::node_reg(node).ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        let grf_base = of::parent_reg_base(node).ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
        let block = block_of(block_base).ok_or_else(|| {
            log::error!("usb2phy: reg base {block_base:#x} matches no known block");
            ProbeError::Fatal(RuntimeError::Invalid)
        })?;

        // Map the whole usb2phy-grf window (0x4000 per the parent's reg
        // size); all block-relative offsets land inside it.
        let grf = dev.res().iomap_region(
            driver_base::MmioRegion::new(grf_base as usize, 0x4000),
            "usb2phy-grf",
        )?;
        let clk480m = dev.res().clk_at(0)?;
        let resets = [dev.res().reset_at(0)?, dev.res().reset_at(1)?];

        // `phy_sus` per block/port from the reference `rk3588_phy_cfgs`.
        let (phy_sus_offset, phy_sus_bit) = match block {
            Block::Usb3_0 | Block::Usb3_1 => (0x000c, 11),
            Block::Usb2_0 | Block::Usb2_1 => (0x0008, 2),
        };

        // Diagnostic: the reference gates the PHY's 480 MHz output behind
        // `clkout_ctl` (block-relative GRF word 0x0000, bit 0: 1 = clock out,
        // 0 = off), opened lazily when a consumer prepares `usb480m_phyN`.
        // Our published clock is a plain FixedRate, so nothing has ever
        // touched this bit — print it (and the port status word) so the
        // board run can say whether the 480 MHz reference ever leaves the PHY.
        {
            let word0 = grf.read32(0x0000);
            log::info!(
                "usb2phy: block {block:?} +0x0000 {word0:#06x} (clkout_ctl bit0 {})",
                if word0 & 0x1 != 0 { "on" } else { "off" },
            );
        }

        // The OTG port owns the detect status word at +0x80/+0x84: bit0 of
        // +0x84 is `ls_det` (a device is on D+/D-), bit1 is `bvalid`. These
        // are the values to compare against the reference board's
        // "device enumerated" state. Read before `grf` moves into `phy`.
        if matches!(block, Block::Usb3_0 | Block::Usb3_1) {
            log::info!(
                "usb2phy: block {block:?} detect +0x80 {:#06x} +0x84 {:#06x} (ls_det {} bvalid {})",
                grf.read32(0x0080),
                grf.read32(0x0084),
                grf.read32(0x0084) & 0x1 != 0,
                grf.read32(0x0084) & 0x2 != 0,
            );
        }

        let phy = Usb2Phy {
            clk480m_enabled: ksync::Mutex::new(None),
            grf,
            block,
            clk480m,
            resets,
            phy_sus_offset,
            phy_sus_bit,
        };

        // `rk3588_usb2phy_tuning` runs at probe (before any consumer).
        phy.tuning();

        // The 480 MHz clock's ops need their own view of the GRF window;
        // take it before `phy` moves into the instance (the provider owns the
        // mapping and lives for the kernel's lifetime, so the borrow is
        // sound).
        let grf_for_clock = phy.grf.clone_shared();

        let instance = PhyInstance::new(Arc::new(phy));
        let lookup: &'static Usb2PhyLookup = Box::leak(Box::new(Usb2PhyLookup {
            provider_id: phandle,
            instance,
        }));
        dev.env()
            .provide::<driver_host::PhyProvider>(phandle, lookup)?;

        // Publish the 480 MHz output under the same phandle. The clock is a
        // real `clkout_ctl`-gated output (see `Usb2Phy480mOps`): the parent's
        // `phyclk` is its reference, and `prepare` opens the GRF gate. The
        // view borrows the GRF mapping via `clone_shared` — the provider owns
        // the mapping and lives for the kernel's lifetime, so the borrow is
        // sound.
        let clock_name: &'static str = firmware.clock_output_name.unwrap_or("usb480m_phy");
        let mut builder = ClkBuilder::new(
            clock_name,
            Arc::new(Usb2Phy480mOps {
                grf: grf_for_clock,
                enabled: AtomicBool::new(false),
            }),
        );
        // The reference parent is the CRU's `phyclk` output
        // (`clk_usb2phy_hdptxrxphy_ref`); `parent_name` resolves it through
        // the global registry even if the CRU driver probes after us.
        builder = builder.parent_name("clk_usb2phy_hdptxrxphy_ref");
        match builder.build() {
            Ok(core) => {
                let lookup: &'static Usb2PhyClockLookup = Box::leak(Box::new(Usb2PhyClockLookup {
                    provider_id: phandle,
                    name: clock_name,
                    core: core.clone(),
                }));
                dev.env()
                    .provide::<driver_host::ClkProvider>(phandle, lookup)?;
                log::info!(
                    "usb2phy: block {block:?} published (phandle {phandle:#x}, grf \
                     {grf_base:#x}), clock `{clock_name}` @ 480 MHz (clkout_ctl gated)",
                );
            }
            Err(err) => {
                log::warn!(
                    "usb2phy: could not build the 480 MHz clock for phandle {phandle:#x}: {err:?}"
                );
            }
        }
        Ok(Usb2PhyData {})
    }
}

driver_base::declare_driver!(PlatformBus, Usb2PhyDriver, "usb2phy", IDS);
