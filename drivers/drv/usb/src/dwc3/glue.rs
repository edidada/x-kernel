// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `snps,dwc3` controller node on Rockchip, host path.
//!
//! The firmware describes two nodes: a parent (`rockchip,rk3588-dwc3`) that
//! carries the clocks and `ranges`, and this child, which carries the register
//! window, the `usb3-otg` reset, the PHYs and the interrupt. The reference
//! splits them across two drivers because it populates children itself; here the
//! platform bus already enumerates every node with a `reg`, so one driver binds
//! the child and reads the clocks from the parent property block — same work,
//! same order, one binding.

use alloc::vec::Vec;

use driver_base::{
    ClkHandle, ClkSpec, Device, DriverOps, OfMatch, PlatformBus, PlatformId, PoweredPhy,
    ProbeError, ResetHandle, RuntimeError,
};
use log::info;

/// `of_device_id`: the generic Synopsys node.
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("snps,dwc3"))];

/// Clock connection names the parent lists, in `clocks` order.
const CLOCKS: [&str; 3] = ["ref", "suspend", "bus"];

pub struct Dwc3Driver;

/// Everything that has to stay alive while the controller is bound: enabled
/// clocks, a deasserted reset and a powered PHY are all refcounted by their
/// handles, so dropping any of them would silently stop the controller.
pub struct Dwc3Data {
    _clocks: Vec<ClkHandle>,
    _reset: ResetHandle,
    _phys: Vec<PoweredPhy>,
    /// The host controller this driver brought up; it owns the register window
    /// the core was programmed through, and enumeration runs on it next.
    _xhci: crate::xhci::Xhci,
}

impl DriverOps<PlatformBus> for Dwc3Driver {
    type Data = Dwc3Data;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<Dwc3Data, ProbeError> {
        let regs = dev.res().iomap(0)?;
        info!(
            "dwc3: probing {:#x}+{:#x}",
            regs.region().base,
            regs.region().size
        );

        // The controller sits in PD_USB: without it the first register access
        // stalls the bus instead of aborting, which is exactly how this port
        // died during bring-up (console silent 83 s right after the PHY came
        // up). Power has to be on before anything touches the window.
        enable_power_domain(dev)?;

        // The parent's `usb3-otg` reset also gates this register window, and
        // the PHY times out if the reset asserts after its clocks come up, so
        // the order is: reset, clocks, PHY, core.
        let reset = dev.res().reset("usb3-otg")?;
        reset.deassert();
        // The block answers on the bus only when this line is really low: the
        // first register read after this point aborts with an external abort
        // (`ESR 0x96000010`) when it is not, so the state is worth a line.
        info!(
            "dwc3: usb3-otg reset deasserted, line asserted {} ",
            reset.status()
        );

        let mut clocks = Vec::new();
        for (index, name) in CLOCKS.iter().enumerate() {
            let Some(spec) = parent_clock_spec(dev, index) else {
                // `suspend` is absent on parts without runtime suspend, and
                // nothing here depends on it.
                info!("dwc3: no parent clock at index {index} ({name}), continuing");
                continue;
            };
            let clock = dev.res().clk_spec(spec)?;
            clock
                .prepare_enable()
                .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
            info!("dwc3: clock {name} enabled at {} Hz", clock.get_rate());
            clocks.push(clock);
        }
        if clocks.is_empty() {
            log::error!("dwc3: parent listed no clocks; the controller cannot move data");
            return Err(ProbeError::Fatal(RuntimeError::NotFound));
        }

        // USB2 PHY only in this stage: High-Speed works through it, and the
        // SuperSpeed PHY is a separate driver that lands later.
        let mut phys = Vec::new();
        let usb2 = usb2_phy_of_port(dev)?;
        phys.push(
            usb2.init_powered()
                .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?,
        );
        info!("dwc3: usb2 phy powered");

        let utmi_wide = dev.fw().string("phy_type") == Some("utmi_wide");
        let dis_enblslpm = dev.fw().flag("snps,dis_enblslpm_quirk");
        let _params = crate::dwc3::core::init(dev, &regs, utmi_wide, dis_enblslpm)?;

        // The host controller takes the window over: the xHCI registers sit at
        // offset 0 of the same mapping the core's global registers live above.
        let xhci = crate::xhci::Xhci::init(dev, regs)?;
        xhci.log_ports();

        Ok(Dwc3Data {
            _clocks: clocks,
            _reset: reset,
            _phys: phys,
            _xhci: xhci,
        })
    }
}

/// Powers on `RK3588_PD_USB`.
///
/// No PM framework exists in this tree yet, so the domain is driven directly,
/// the same way the PCIe host does it for `RK3588_PD_PCIE` (both live in the
/// PMU at 0xfd8d8000 and both take a pwr bit in register 0x150). Values come
/// from pm-domains.c's rk3588 table: domain 31 has `pwr_offset 0x4`,
/// `pwr_mask BIT(11)`, and the write is the hiword-masked form.
fn enable_power_domain(dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
    const PMU_BASE: u64 = 0xfd8d_8000;
    const PD_PWR_CON: usize = 0x150;
    const PD_USB_BIT: u32 = 1 << 11;
    let pmu = dev.res().iomap_region(
        driver_base::MmioRegion::new(PMU_BASE as usize, 0x400),
        "usb-pmu",
    )?;
    if pmu.read32(PD_PWR_CON) & PD_USB_BIT != 0 {
        info!("dwc3: PD_USB already on ({:#010x})", pmu.read32(PD_PWR_CON));
        return Ok(());
    }
    pmu.write32(PD_PWR_CON, (PD_USB_BIT << 16) | PD_USB_BIT);
    for _ in 0..10_000 {
        if pmu.read32(PD_PWR_CON) & PD_USB_BIT != 0 {
            info!("dwc3: PD_USB powered on ({:#010x})", pmu.read32(PD_PWR_CON));
            return Ok(());
        }
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1));
    }
    log::error!(
        "dwc3: PD_USB did not come up (pwr_con {:#010x})",
        pmu.read32(PD_PWR_CON),
    );
    Err(ProbeError::Fatal(RuntimeError::Io))
}

/// Acquires the USB2 PHY a `phys` entry of this node points at.
///
/// The firmware references the PHY **port** node (`otg-port`, `host-port`),
/// which carries the port's own interrupt and supply, while the driver that
/// publishes the PHY binds the enclosing `usb2-phy@X` node. Requiring the two to
/// be the same phandle is what parked USB at "deferred forever" during bring-up
/// (`device 420 still deferred on phy:426`), so the port is resolved to its
/// parent here. The provider should publish one instance per port -- that is
/// also what per-port suspend/otg-host configuration needs -- and this mapping
/// goes away when it does.
fn usb2_phy_of_port(dev: &Device<'_, PlatformBus>) -> Result<driver_base::PhyHandle, ProbeError> {
    let spec = dev
        .res()
        .map()
        .phy_by_name("usb2-phy")
        .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
    let port_phandle = spec.provider;
    let port = of::find_node_by_phandle(port_phandle as u32)
        .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
    let parent_phandle = port
        .parent_property_u32("phandle")
        .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
    info!("dwc3: usb2 phy port {port_phandle:#x} resolved to its block {parent_phandle:#x}");
    dev.res().phy_spec(driver_base::PhySpec::by_phandle_cells(
        u64::from(parent_phandle),
        spec.cells,
        spec.ncells,
    ))
}

/// The n-th clock the parent node lists.
///
/// The child's own `clocks` property is empty by construction — the reference
/// glue owns them — so the specs come from the parent block. `#clock-cells` is
/// one cell for every provider on this SoC, which is the shape the property
/// walk below assumes.
fn parent_clock_spec(dev: &Device<'_, PlatformBus>, index: usize) -> Option<ClkSpec> {
    let phandle = {
        let fw = dev.fw();
        fw.node()?.phandle?
    };
    let fdt_node = of::find_node_by_phandle(phandle)?;
    let raw = fdt_node.parent_property("clocks")?.value;
    if raw.len() < 8 {
        return None;
    }
    let cells: Vec<u32> = raw
        .chunks_exact(4)
        .map(|cell| u32::from_be_bytes(cell.try_into().unwrap_or([0; 4])))
        .collect();
    // Two cells per entry: phandle, index.
    let provider = *cells.get(index * 2)?;
    let clock_index = *cells.get(index * 2 + 1)?;
    Some(ClkSpec::by_phandle_index(u64::from(provider), clock_index))
}

driver_base::declare_driver!(PlatformBus, Dwc3Driver, "dwc3", IDS);
