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

use driver_base::{
    ClkHandle, Device, DriverOps, IoMap, OfMatch, PlatformBus, PlatformId, ProbeError, ResetHandle,
    RuntimeError,
};
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
    /// The block base inside the grf window (the node's own `reg` value):
    /// every tuning/`phy_sus` offset in the reference is block-relative.
    block_base: usize,
    block: Block,
    clk480m: ClkHandle,
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
        let base = self.block_base;
        // utmi_termselect = 1'b1 (FS terminations),
        // utmi_xcvrselect = 2'b01 (FS transceiver);
        // USB3 blocks add utmi_opmode = 2'b01 (no-driving).
        let suspend_cfg: u32 = match self.block {
            Block::Usb3_0 | Block::Usb3_1 => 0x14 | 0x01,
            Block::Usb2_0 | Block::Usb2_1 => 0x14,
        };

        // Deassert SIDDQ to power on the analog block: GENMASK(29,29).
        self.grf.write32(base + 0x0008, 1 << 29);
        // Do reset after exit IDDQ mode.
        self.reset_pulse();
        // Suspend configuration: GENMASK(20,16) | suspend_cfg.
        self.grf.write32(base + 0x000c, (0x1f << 16) | suspend_cfg);
        // HS DC Voltage Level Adjustment 4'b1001: +5.89%.
        self.grf.write32(base + 0x0004, (0xf << 24) | 0x0900);
        // HS Transmitter Pre-Emphasis Current Control 2'b10: 2x.
        self.grf.write32(base + 0x0008, (0x3 << 19) | 0x0010);
    }

    /// `phy_sus` suspend-bit write (hiword convention): `false` = power
    /// on, `true` = suspend.
    fn set_suspended(&self, suspended: bool) {
        let mask = 1u32 << self.phy_sus_bit;
        let tmp: u32 = if suspended { 1 } else { 0 };
        self.grf.write32(
            self.block_base + self.phy_sus_offset,
            (tmp << self.phy_sus_bit) | (mask << 16),
        );
    }
}

impl PhyHwOps for Usb2Phy {
    /// HOST-mode `rockchip_usb2phy_init`: the reference arms the
    /// linestate/disconnect detect IRQs — out of the v1 surface (see the
    /// module docs); the phy functions without them (consumers poll).
    fn hw_init(&self) -> Result<(), PhyError> {
        Ok(())
    }

    /// `rockchip_usb2phy_exit`: nothing beyond the power pair for HOST.
    fn hw_exit(&self) -> Result<(), PhyError> {
        Ok(())
    }

    /// `rockchip_usb2phy_power_on`: 480m clock + `phy_sus` cleared.
    fn hw_power_on(&self) -> Result<(), PhyError> {
        self.clk480m.prepare_enable().map_err(|_| PhyError::Io)?;
        self.set_suspended(false);
        Ok(())
    }

    /// `rockchip_usb2phy_power_off`: `phy_sus` set + clock gated.
    fn hw_power_off(&self) {
        self.set_suspended(true);
        self.clk480m.disable_unprepare();
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

        let phy = Usb2Phy {
            grf,
            block_base: block_base as usize,
            block,
            clk480m,
            resets,
            phy_sus_offset,
            phy_sus_bit,
        };

        // `rk3588_usb2phy_tuning` runs at probe (before any consumer).
        phy.tuning();

        let instance = PhyInstance::new(Arc::new(phy));
        let lookup: &'static Usb2PhyLookup = Box::leak(Box::new(Usb2PhyLookup {
            provider_id: phandle,
            instance,
        }));
        dev.env()
            .provide::<driver_host::PhyProvider>(phandle, lookup)?;

        log::info!("usb2phy: block {block:?} published (phandle {phandle:#x}, grf {grf_base:#x})");
        Ok(Usb2PhyData {})
    }
}

driver_base::declare_driver!(PlatformBus, Usb2PhyDriver, "usb2phy", IDS);
