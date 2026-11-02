// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RK3588 DesignWare PCIe root-complex host (pcie-dw-rockchip).
//!
//! Faithful translation of the pinned references
//! `pcie-dw-rockchip.c` + `pcie-designware{,-host}.c` (linux-7.1.8),
//! v1 surface:
//!
//! - probe: 6 clocks, the CRU reset pair (req-reset), the phy through
//!   the kphy registry (req-phy), the `pcie-apb`/`pcie-dbi` windows,
//!   and the pipe-grf;
//! - RC configure: client LTSSM-enhance + mode RC (APB hiword writes),
//!   then the DW core setup — DBI RO-write enable, link fields, RC
//!   BARs, bus numbers, command register, class = PCI bridge;
//! - outbound iATU: the DT `ranges` MEM windows plus one CFG window
//!   carved from the first (IO/CFG) window — the mainline "config" reg
//!   the BSP dts lacks as a named entry;
//! - config access: the root complex itself reads/writes directly on
//!   the DBI slice; remote functions via iATU CFG0 (bus 1, type 0) /
//!   CFG1 (deeper, type 1) before every access;
//! - link-up poll (LTSSM status on the APB view), then a bus 0/1 scan
//!   logging every responding function's vendor:device — the
//!   enumeration evidence.
//!
//! v1 gaps (tracked): device descriptors do not enter the domain yet
//! (INTx routing via interrupt-map + BAR windows land with the next
//! slice), no MSI (INTx-class devices only), IO-space windows not
//! programmed, deeper-than-bus-1 switches untested.

use alloc::{sync::Arc, vec::Vec};

use driver_base::{
    Device, DriverOps, IoMap, OfMatch, PlatformBus, PlatformId, ProbeError, ResetHandle,
    RuntimeError,
};
use kspin::SpinNoIrq;

use crate::{ConfigurationAccess, DeviceFunction, PciRoot};

// ---- APB (client) registers — hiword write-mask convention ---------------
const PCIE_CLIENT_GENERAL_CON: usize = 0x0;
const PCIE_CLIENT_MODE_RC: u32 = 0x4; // FIELD value in bits 7:4
const PCIE_CLIENT_MODE_MASK: u32 = 0xf << 4;
const PCIE_CLIENT_ENABLE_LTSSM: u32 = 0x1 << 2;
const PCIE_CLIENT_LTSSM_BIT: u32 = 0x1 << 2;
const PCIE_CLIENT_HOT_RESET_CTRL: usize = 0x180;
const PCIE_LTSSM_ENABLE_ENHANCE: u32 = 0x1 << 4;
const PCIE_CLIENT_LTSSM_STATUS: usize = 0x300;
const PCIE_LINKUP_MASK: u32 = 0x3 << 16;
const PCIE_LINKUP: u32 = 0x3;

/// `FIELD_PREP_WM16(mask, value)`: hiword write — value in the low half,
/// mask in the high half.
const fn wm16(mask: u32, value: u32) -> u32 {
    ((value & mask) << 16) | (value & mask)
}

// ---- DBI (port logic + config space) registers ---------------------------
const PCIE_PORT_LINK_CONTROL: usize = 0x710;
const PORT_LINK_DLL_LINK_EN: u32 = 1 << 5;
const PORT_LINK_FAST_LINK_MODE: u32 = 1 << 7;
const PCIE_LINK_WIDTH_SPEED_CONTROL: usize = 0x80c;
const PORT_LOGIC_SPEED_CHANGE: u32 = 1 << 17;
const PCIE_MISC_CONTROL_1_OFF: usize = 0x8bc;
const PCIE_DBI_RO_WR_EN: u32 = 1 << 0;

// The RC's own config-space header (read/written directly on the DBI).
const PCI_COMMAND: usize = 0x04;
const PCI_CLASS_DEVICE: usize = 0x0a;
const PCI_PRIMARY_BUS: usize = 0x18;
const PCI_BASE_ADDRESS_0: usize = 0x10;
const PCI_INTERRUPT_PIN: usize = 0x3c;
const PCI_CLASS_BRIDGE_PCI: u32 = 0x0604;

const PCI_COMMAND_IO: u32 = 1 << 0;
const PCI_COMMAND_MEMORY: u32 = 1 << 1;
const PCI_COMMAND_MASTER: u32 = 1 << 2;
const PCI_COMMAND_SERR: u32 = 1 << 8;

const PCI_CAP_PTR: usize = 0x34;
const PCI_EXP_CAP_ID: u8 = 0x10;
const PCI_EXP_LNKCTL2: usize = 0x30; // + cap base

// ---- iATU (unrolled, dbi + DEFAULT_DBI_ATU_OFFSET) -----------------------
const DEFAULT_DBI_ATU_OFFSET: usize = 0x3 << 20; // 0x300000
const PCIE_ATU_REGION_CTRL1: usize = 0x000;
const PCIE_ATU_REGION_CTRL2: usize = 0x004;
const PCIE_ATU_ENABLE: u32 = 1 << 31;
const PCIE_ATU_LOWER_BASE: usize = 0x008;
const PCIE_ATU_UPPER_BASE: usize = 0x00c;
const PCIE_ATU_LIMIT: usize = 0x010;
const PCIE_ATU_LOWER_TARGET: usize = 0x014;
const PCIE_ATU_UPPER_TARGET: usize = 0x018;
const PCIE_ATU_TYPE_MEM: u32 = 0x0;
const PCIE_ATU_TYPE_CFG0: u32 = 0x4;
const PCIE_ATU_TYPE_CFG1: u32 = 0x5;

/// One DT `ranges` window, classified.
#[derive(Debug, Clone, Copy)]
struct PcieWindow {
    pci_base: u64,
    cpu_base: u64,
    size: u64,
    io: bool,
}

/// Parse the node's `ranges` for the rk3588 shape: child 3 cells
/// (`#address-cells = <3>` on the PCI node), parent 2 cells (the /soc
/// `#address-cells`), size 2 cells — 7 cells per window.
fn parse_ranges(cells: &[u32]) -> Vec<PcieWindow> {
    const STRIDE: usize = 7; // child 3 + parent 2 + size 2
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + STRIDE <= cells.len() {
        let flags = cells[i] >> 24;
        out.push(PcieWindow {
            pci_base: (u64::from(cells[i + 1]) << 32) | u64::from(cells[i + 2]),
            cpu_base: (u64::from(cells[i + 3]) << 32) | u64::from(cells[i + 4]),
            size: (u64::from(cells[i + 5]) << 32) | u64::from(cells[i + 6]),
            io: flags & 0x1 != 0,
        });
        i += STRIDE;
    }
    out
}

/// The configured root: every acquired window/handle lives here for the
/// boot lifetime (PCI host bridges never unbind).
struct DwPcie {
    apb: IoMap,
    dbi: IoMap,
    cfg_win: IoMap,
    cfg_base: u64,
    num_ob_windows: usize,
    max_speed: u32,
    /// Read from DT, carried for the link-up diagnostics.
    num_lanes: u32,
}

impl DwPcie {
    /// Lanes as configured (link-up diagnostics).
    fn lanes(&self) -> u32 {
        self.num_lanes
    }

    fn link_up(&self) -> bool {
        (self.apb.read32(PCIE_CLIENT_LTSSM_STATUS) & PCIE_LINKUP_MASK) >> 16 == PCIE_LINKUP
    }

    fn dbi_read(&self, reg: usize) -> u32 {
        self.dbi.read32(reg)
    }

    fn dbi_write(&self, reg: usize, value: u32) {
        self.dbi.write32(reg, value);
    }

    fn dbi_writew(&self, reg: usize, value: u16) {
        let aligned = reg & !0x3;
        let shift = u32::try_from(reg & 0x2).unwrap_or(0) * 8;
        let merged = (self.dbi.read32(aligned) & !(0xffff << shift)) | (u32::from(value) << shift);
        self.dbi.write32(aligned, merged);
    }

    fn atu_base(&self, index: usize) -> usize {
        DEFAULT_DBI_ATU_OFFSET + (index << 9)
    }

    /// `dw_pcie_prog_outbound_atu` (unrolled; MEM/CFG types, no routing).
    fn prog_outbound_atu(
        &self,
        index: usize,
        type_: u32,
        cpu_base: u64,
        size: u64,
        pci_base: u64,
    ) -> bool {
        if index >= self.num_ob_windows || size == 0 {
            return false;
        }
        let limit = cpu_base + size - 1;
        self.dbi_write(self.atu_base(index) + PCIE_ATU_LOWER_BASE, cpu_base as u32);
        self.dbi_write(
            self.atu_base(index) + PCIE_ATU_UPPER_BASE,
            (cpu_base >> 32) as u32,
        );
        self.dbi_write(self.atu_base(index) + PCIE_ATU_LIMIT, limit as u32);
        self.dbi_write(
            self.atu_base(index) + PCIE_ATU_LOWER_TARGET,
            pci_base as u32,
        );
        self.dbi_write(
            self.atu_base(index) + PCIE_ATU_UPPER_TARGET,
            (pci_base >> 32) as u32,
        );
        self.dbi_write(self.atu_base(index) + PCIE_ATU_REGION_CTRL1, type_);
        self.dbi_write(
            self.atu_base(index) + PCIE_ATU_REGION_CTRL2,
            PCIE_ATU_ENABLE,
        );
        // Enable read-back (the reference polls; one read suffices — the
        // enable is not hardware-gated).
        self.dbi_read(self.atu_base(index) + PCIE_ATU_REGION_CTRL2) & PCIE_ATU_ENABLE != 0
    }

    /// `dw_pcie_link_set_max_speed`: LNKCTL2 target speed from the DT
    /// `max-link-speed` (1=2.5GT, 2=5GT, ...; 0 = unchanged).
    fn link_set_max_speed(&self) {
        let speed = match self.max_speed {
            1..=5 => self.max_speed,
            _ => return,
        };
        let Some(cap) = self.pcie_cap_base() else {
            return;
        };
        let lnkctl2 = cap + PCI_EXP_LNKCTL2;
        let val = self.dbi_read(lnkctl2);
        self.dbi_write(lnkctl2, (val & !0xf) | speed);
    }

    /// Walk the capability list for the PCIe capability (id 0x10).
    fn pcie_cap_base(&self) -> Option<usize> {
        let mut cap = (self.dbi_read(PCI_CAP_PTR) as u8 & 0xfc) as usize;
        for _ in 0..16 {
            if cap == 0 {
                return None;
            }
            let val = self.dbi_read(cap);
            if val as u8 == PCI_EXP_CAP_ID {
                return Some(cap);
            }
            cap = ((val >> 8) as u8 & 0xfc) as usize;
        }
        None
    }

    /// `dw_pcie_setup_rc` (v1 subset: no MSI, no presets/L1ss) plus
    /// `dw_pcie_setup`'s link fields.
    fn setup_rc(&self) {
        // DBI read-only registers become writable.
        self.dbi_write(PCIE_MISC_CONTROL_1_OFF, PCIE_DBI_RO_WR_EN);

        // dw_pcie_setup: max speed + link fields.
        self.link_set_max_speed();
        let val = self.dbi_read(PCIE_PORT_LINK_CONTROL);
        self.dbi_write(
            PCIE_PORT_LINK_CONTROL,
            (val & !PORT_LINK_FAST_LINK_MODE) | PORT_LINK_DLL_LINK_EN,
        );
        let val = self.dbi_read(PCIE_LINK_WIDTH_SPEED_CONTROL);
        self.dbi_write(PCIE_LINK_WIDTH_SPEED_CONTROL, val | PORT_LOGIC_SPEED_CHANGE);

        // RC BARs (the reference: BAR0 = 4, the 64-bit flag; cleared after
        // the iATU setup).
        self.dbi_write(PCI_BASE_ADDRESS_0, 0x4);
        self.dbi_write(PCI_BASE_ADDRESS_0 + 0x4, 0x0);

        // Interrupt pin = INTA.
        let val = self.dbi_read(PCI_INTERRUPT_PIN);
        self.dbi_write(PCI_INTERRUPT_PIN, (val & 0xffff_00ff) | 0x0000_0100);

        // Bus numbers: primary 0, secondary 1, subordinate 0xff.
        let val = self.dbi_read(PCI_PRIMARY_BUS);
        self.dbi_write(PCI_PRIMARY_BUS, (val & 0xff00_0000) | 0x00ff_0100);

        // Command register: IO + MEM + MASTER + SERR.
        let val = self.dbi_read(PCI_COMMAND);
        self.dbi_write(
            PCI_COMMAND,
            (val & 0xffff_0000)
                | PCI_COMMAND_IO
                | PCI_COMMAND_MEMORY
                | PCI_COMMAND_MASTER
                | PCI_COMMAND_SERR,
        );

        // Program correct class for RC.
        self.dbi_writew(PCI_CLASS_DEVICE, PCI_CLASS_BRIDGE_PCI as u16);
    }

    /// `dw_pcie_iatu_setup` (v1 subset): MEM windows from the DT ranges;
    /// IO windows skipped (no legacy-IO consumer). Index 0 stays reserved
    /// for the CFG window.
    fn iatu_setup(&self, windows: &[PcieWindow]) {
        let mut index = 1usize;
        for win in windows {
            if win.io || win.size == 0 || index >= self.num_ob_windows {
                continue;
            }
            if !self.prog_outbound_atu(
                index,
                PCIE_ATU_TYPE_MEM,
                win.cpu_base,
                win.size,
                win.pci_base,
            ) {
                break;
            }
            index += 1;
        }
    }
}

/// The `ConfigurationAccess` over the configured root: own-conf (0:0:0)
/// reads/writes directly on the DBI; remote functions reprogram the CFG
/// iATU window per access (type 0 on bus 1, type 1 deeper) and go
/// through the config window alias.
#[derive(Clone)]
struct DwCam {
    hw: Arc<DwPcie>,
    // Serialize iATU reprogramming vs concurrent config access (single
    // CFG window shared by all functions).
    lock: Arc<SpinNoIrq<()>>,
}

impl DwCam {
    /// Program the CFG iATU window for `bdf` (link-up last line of
    /// defense, matching the reference).
    fn map_bus(&self, bdf: DeviceFunction) -> bool {
        if !self.hw.link_up() {
            return false;
        }
        let type_ = if bdf.bus == 1 {
            PCIE_ATU_TYPE_CFG0
        } else {
            PCIE_ATU_TYPE_CFG1
        };
        let busdev =
            (u32::from(bdf.bus) << 8) | (u32::from(bdf.device) << 3) | u32::from(bdf.function);
        self.hw
            .prog_outbound_atu(0, type_, self.hw.cfg_base, 0x1000, u64::from(busdev))
    }

    /// Own-conf (0:0:0) and header-critical reads stay on the DBI.
    fn is_own(&self, bdf: DeviceFunction, offset: u8) -> bool {
        bdf.bus == 0 && bdf.device == 0 && bdf.function == 0 && offset <= 0xfc
    }
}

impl ConfigurationAccess for DwCam {
    // read_word/write_word per the vendored trait.
    fn read_word(&self, bdf: DeviceFunction, register_offset: u8) -> u32 {
        let reg = register_offset as usize;
        if self.is_own(bdf, register_offset) {
            return self.hw.dbi_read(reg);
        }
        let _guard = self.lock.lock();
        if !self.map_bus(bdf) {
            return u32::MAX;
        }
        self.hw.cfg_win.read32(reg)
    }

    fn write_word(&mut self, bdf: DeviceFunction, register_offset: u8, data: u32) {
        let reg = register_offset as usize;
        if self.is_own(bdf, register_offset) {
            self.hw.dbi_write(reg, data);
            return;
        }
        let _guard = self.lock.lock();
        if !self.map_bus(bdf) {
            return;
        }
        self.hw.cfg_win.write32(reg, data);
    }

    unsafe fn unsafe_clone(&self) -> Self {
        Self {
            hw: self.hw.clone(),
            lock: self.lock.clone(),
        }
    }
}

/// The v1 enumeration evidence: scan bus 0/1, log each responding
/// function's vendor:device (domain registration lands next slice).
fn scan_and_log(root: &PciRoot<DwCam>, cam: &DwCam) {
    for bus in [0u8, 1u8] {
        for (bdf, _info) in root.enumerate_bus(bus) {
            let word = cam.read_word(bdf, 0x00);
            log::info!(
                "pcie-dw: bus {bus} dev {:02x}.{} vendor {:#06x} device {:#06x}",
                bdf.device,
                bdf.function,
                word & 0xffff,
                word >> 16
            );
        }
    }
}

/// Firmware facts for one root port: lane/speed config, the outbound
/// window count, the parsed `ranges` and the CFG window.
fn parse_facts(
    dev: &mut Device<'_, PlatformBus>,
) -> Result<(u32, u32, usize, Vec<PcieWindow>, u64), ProbeError> {
    let fw = dev.fw();
    let firmware = fw.node().ok_or(ProbeError::Fatal(RuntimeError::Invalid))?;
    let num_lanes = firmware
        .prop("num-lanes")
        .and_then(|p| p.as_u32())
        .unwrap_or(1);
    let max_speed = firmware
        .prop("max-link-speed")
        .and_then(|p| p.as_u32())
        .unwrap_or(0);
    let num_ob_windows = firmware
        .prop("num-ob-windows")
        .and_then(|p| p.as_u32())
        .map(|v| v as usize)
        .unwrap_or(8);
    let windows = firmware
        .prop("ranges")
        .and_then(|p| p.u32_list())
        .map(|cells| parse_ranges(&cells))
        .unwrap_or_default();
    for win in &windows {
        log::info!(
            "pcie-dw: range cpu {:#x}+{:#x} -> pci {:#x}{}",
            win.cpu_base,
            win.size,
            win.pci_base,
            if win.io { " (io)" } else { "" }
        );
    }
    // The config window: the first (IO/CFG) range — the mainline
    // "config" reg is that window's CPU base.
    let cfg = windows
        .iter()
        .find(|w| w.io)
        .ok_or_else(|| {
            log::error!("pcie-dw: no IO/CFG window in ranges");
            ProbeError::Fatal(RuntimeError::NotFound)
        })?
        .cpu_base;
    Ok((num_lanes, max_speed, num_ob_windows, windows, cfg))
}

/// `static const struct of_device_id rockchip_pcie_of_match[]` (RC).
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("rockchip,rk3588-pcie"))];

/// Driver-private data (boot-static host bridge).
pub struct DwHostData {}

/// The platform driver.
pub struct DwHostDriver;

/// RK3588_PD_PCIE power domain (domain 34, PMU @ 0xfd8d8000): the
/// controller sits in a power domain the mainline dts declares
/// (`power-domains = <&power RK3588_PD_PCIE>`; the BSP dts omits it).
/// No PM framework exists yet — this drives the PD power-on sequence
/// directly (pm-domains.c semantics: pwr_offset 0x14c, repair status
/// 0x290 bit 22) and records the PM-framework gap.
fn enable_pcie_power_domain(dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
    log::info!("pcie-dw: enabling the pcie power domain (PD_PCIE)");
    const PMU_BASE: u64 = 0xfd8d_8000;
    const PD_PWR_CON: usize = 0x14c; // pwr_offset (rk3588_pmu_info)
    const PD_REPAIR_STATUS: usize = 0x290; // repair_status_offset
    const PD_PWR_BIT: u32 = 1 << 7; // domain 34's pwr/status bit
    const PD_REPAIR_BIT: u32 = 1 << 22; // 1 = powered
    let pmu = dev.res().iomap_region(
        driver_base::MmioRegion::new(PMU_BASE as usize, 0x400),
        "pcie-pmu",
    )?;
    if pmu.read32(PD_REPAIR_STATUS) & PD_REPAIR_BIT != 0 {
        log::info!("pcie-dw: PD_PCIE already on");
        return Ok(());
    }
    // Hiword write: enable bit 7, request it to 0 (power on).
    pmu.write32(PD_PWR_CON, PD_PWR_BIT << 16);
    for _ in 0..10_000 {
        if pmu.read32(PD_REPAIR_STATUS) & PD_REPAIR_BIT != 0 {
            log::info!("pcie-dw: PD_PCIE powered on");
            return Ok(());
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1));
    }
    log::error!("pcie-dw: PD_PCIE failed to power up");
    Err(ProbeError::Fatal(RuntimeError::Io))
}

impl DriverOps<PlatformBus> for DwHostDriver {
    type Data = DwHostData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<DwHostData, ProbeError> {
        // `rockchip_pcie_resource_get`: the two windows + the reset pair.
        let apb = dev.res().iomap_named("pcie-apb")?;
        let dbi = dev.res().iomap_named("pcie-dbi")?;
        let resets: [ResetHandle; 2] = [dev.res().reset_at(0)?, dev.res().reset_at(1)?];

        // Bind the view: `node()` borrows the firmware snapshot.
        let (num_lanes, max_speed, num_ob_windows, windows, cfg_base) = parse_facts(dev)?;
        let cfg_size = windows
            .iter()
            .find(|w| w.io)
            .map(|w| w.size)
            .unwrap_or(0x10_0000);

        // BSP-aligned order (rk_pcie_really_probe): phy init+power first,
        // then the reset pair DEASSERTED ONLY (the BSP never asserts at
        // probe — the power-on default is the reset state), then clocks,
        // then the first DBI access.
        let phy = dev.res().phy("pcie-phy")?;
        let powered = phy.init_powered()?;
        log::info!("pcie-dw: phy powered, deasserting resets");
        for rst in &resets {
            rst.deassert();
        }
        log::info!("pcie-dw: resets deasserted, enabling clocks");
        for name in ["aclk_mst", "aclk_slv", "aclk_dbi", "pclk", "aux", "pipe"] {
            log::info!("pcie-dw: enabling clock {name}");
            let clk = dev.res().clk(name)?;
            clk.enable()?;
        }
        enable_pcie_power_domain(dev)?;

        log::info!("pcie-dw: clocks on, configuring client (APB)");
        // `rk_pcie_fast_link_setup`: RMW — LTSSM EN control mode. The APB
        // sequence comes BEFORE the first DBI access in the BSP flow (the
        // client mode gates the DBI slice's responsiveness).
        let val = apb.read32(PCIE_CLIENT_HOT_RESET_CTRL);
        apb.write32(
            PCIE_CLIENT_HOT_RESET_CTRL,
            val | PCIE_LTSSM_ENABLE_ENHANCE | (PCIE_LTSSM_ENABLE_ENHANCE << 16),
        );
        // `rk_pcie_set_mode` (RC, no supports-clkreq): pull CLKREQ# down
        // to assert the connecting CLOCK_GEN OE (without it the link can
        // never train), then mode RC (LTSSM still off — the enable write
        // comes after the setup).
        const PCIE_CLIENT_POWER: usize = 0x2c;
        apb.write32(PCIE_CLIENT_POWER, 0x3001_1000);
        apb.write32(PCIE_CLIENT_GENERAL_CON, 0xf000_0040);

        log::info!("pcie-dw: client configured, probing dbi");
        // Diagnostics: the client LTSSM status (proven responsive) and the
        // fe190000+0 word — mainline treats THIS window as the DBI, the BSP
        // as the client regs; the vendor id appearing here disambiguates.
        let ltssm = apb.read32(PCIE_CLIENT_LTSSM_STATUS);
        log::info!("pcie-dw: client ltssm status {ltssm:#010x}");
        let apb_word = apb.read32(0x00);
        log::info!("pcie-dw: fe190000+0 word {apb_word:#010x}");
        // The BSP's first DBI access lands here (dbi_ro_wr_en): read the
        // vendor id first so a wedged DBI names itself in the log.
        let vendor_word = dbi.read32(0x00);
        log::info!(
            "pcie-dw: dbi alive, vendor {:#06x} device {:#06x}",
            vendor_word & 0xffff,
            vendor_word >> 16
        );

        // The config window maps the first range's CPU base.
        let cfg_win = dev.res().iomap_region(
            driver_base::MmioRegion::new(cfg_base as usize, cfg_size as usize),
            "pcie-cfg",
        )?;

        log::info!("pcie-dw: resources acquired, configuring RC");
        let pcie = Arc::new(DwPcie {
            apb,
            dbi,
            cfg_win,
            cfg_base,
            num_ob_windows: num_ob_windows.max(2),
            max_speed,
            num_lanes,
        });

        log::info!("pcie-dw: setup_rc (dbi writes)");
        pcie.setup_rc();
        log::info!("pcie-dw: setup_rc done, programming iATU");
        pcie.iatu_setup(&windows);
        log::info!("pcie-dw: iATU programmed, enabling LTSSM");

        // LTSSM enable: the reference's post-setup write.
        // LTSSM enable (after setup; the reference's post-setup write).
        pcie.apb.write32(
            PCIE_CLIENT_GENERAL_CON,
            wm16(PCIE_CLIENT_MODE_MASK, PCIE_CLIENT_MODE_RC)
                | wm16(PCIE_CLIENT_LTSSM_BIT, PCIE_CLIENT_ENABLE_LTSSM),
        );

        // `dw_pcie_wait_for_link`: up to ~1s for LTSSM to reach L0.
        log::info!("pcie-dw: polling for link up");
        for _ in 0..100 {
            if pcie.link_up() {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_millis(10));
        }
        if !pcie.link_up() {
            log::warn!("pcie-dw: link never came up (empty slot?)");
            // An empty slot is a board fact, not a driver failure: the
            // controller stays configured for the next boot stage.
            return Ok(DwHostData {});
        }
        log::info!("pcie-dw: link up, {} lane(s)", pcie.lanes());

        // v1 enumeration evidence: scan bus 0 (the RC itself) and bus 1,
        // logging every responding function's vendor:device.
        let cam = DwCam {
            hw: pcie.clone(),
            lock: Arc::new(SpinNoIrq::new(())),
        };
        let root = PciRoot::new(cam.clone());
        scan_and_log(&root, &cam);

        // The guard keeps the phy powered for the binding lifetime; the
        // clocks/resets/iomaps stay for the boot lifetime.
        core::mem::forget(powered);
        Ok(DwHostData {})
    }
}

driver_base::declare_driver!(PlatformBus, DwHostDriver, "pcie-dw-host", IDS);
