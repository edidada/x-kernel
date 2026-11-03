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
    ClkHandle, ClkSpec, Device, DriverOps, EnabledClock, OfMatch, PlatformBus, PlatformId,
    PoweredPhy, ProbeError, ResetHandle, RuntimeError,
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
    /// One guard per enabled clock: dropping it releases the enable again, so a
    /// `prepare_enable()` whose result is discarded switches the clock straight
    /// back off (and, with `SET_TO_DISABLE` gates, leaves the gate bit set --
    /// which is exactly how this driver spent a bring-up round gating off the
    /// controller it was trying to start).
    _clock_guards: Vec<EnabledClock>,
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
        let mut clock_guards = Vec::new();
        for (index, name) in CLOCKS.iter().enumerate() {
            let Some(spec) = parent_clock_spec(dev, index) else {
                // `suspend` is absent on parts without runtime suspend, and
                // nothing here depends on it.
                info!("dwc3: no parent clock at index {index} ({name}), continuing");
                continue;
            };
            let clock = dev.res().clk_spec(spec)?;
            let before = clock_gate_word(dev);
            let guard = clock
                .prepare_enable()
                .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
            let after = clock_gate_word(dev);
            info!(
                "dwc3: clock {name} (id {:#x}) at {} Hz, gates {before:#010x} -> {after:#010x}",
                spec.index,
                clock.get_rate(),
            );
            clocks.push(clock);
            clock_guards.push(guard);
        }
        if clocks.is_empty() {
            log::error!("dwc3: parent listed no clocks; the controller cannot move data");
            return Err(ProbeError::Fatal(RuntimeError::NotFound));
        }

        log_cru_clocks(dev);

        // USB2 PHY only in this stage: High-Speed works through it, and the
        // SuperSpeed PHY is a separate driver that lands later.
        let mut phys = Vec::new();
        let usb2 = usb2_phy_of_port(dev)?;
        phys.push(
            usb2.init_powered()
                .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?,
        );
        info!("dwc3: usb2 phy powered");

        // The USB3 port has to be taken out of "disabled" in the USB GRF before
        // the controller can see anything on it: PIPE_ENABLE=1, PHY_DISABLE=0,
        // SUSPEND_ENABLE=0, U3_PORT_DISABLE=0. The reference implementations
        // (Linux's usbdp-phy/u-boot `udphy_u3_port_disable(port, false)`, and the
        // tgoskits USB host port) all do this step; without it the port stays
        // dark and a device plugged into it never shows up as a connection.
        enable_u3_port(dev, regs.region().base);

        let utmi_wide = dev.fw().string("phy_type") == Some("utmi_wide");
        let dis_enblslpm = dev.fw().flag("snps,dis_enblslpm_quirk");
        let _params = crate::dwc3::core::init(dev, &regs, utmi_wide, dis_enblslpm)?;

        // The host controller takes the window over: the xHCI registers sit at
        // offset 0 of the same mapping the core's global registers live above.
        let mut xhci = crate::xhci::Xhci::init(dev, regs)?;
        xhci.log_ports();
        probe_ehci_ports(dev);
        // Enumerate what is already attached. The controller runs on its own
        // after this, so a device that appears later needs interrupt delivery,
        // which this port does not have yet.
        let (found, ready) = xhci.enumerate(dev);
        info!("dwc3: enumeration found {found} device(s), {ready} usable");

        Ok(Dwc3Data {
            _clocks: clocks,
            _clock_guards: clock_guards,
            _reset: reset,
            _phys: phys,
            _xhci: xhci,
        })
    }
}

/// Powers on `RK3588_PD_USB` and switches the ports' 5V on.
///
/// No PM framework exists in this tree yet, so the domain is driven directly out
/// of pm-domains.c's rk3588 table (domain 31: `pwr_offset 0x4`, `pwr_mask`
/// `BIT(11)`, in the PMU at 0xfd8d8000).
///
/// **The polarity is inverted**: "powered" is the bit CLEAR, and the power-up
/// write is `pwr_w_mask` alone (`BIT(11) << 16`, low half zero). A working
/// reference board reads 0x150 as `0x000004c1`, i.e. bit 11 clear. Asserting it
/// -- which this driver did first -- requests a power-down, and the symptom is
/// the worst kind: the controller stops answering on the bus, so the first
/// register access raises a synchronous external abort instead of reporting an
/// error.
fn enable_power_domain(dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
    const PMU_BASE: u64 = 0xfd8d_8000;
    const PD_PWR_CON: usize = 0x150;
    const PD_MEM_PWR: usize = 0x1a4;
    // The authoritative "is it on" bit: repair_status, where 1 means powered
    // (pm-domains' rockchip_pmu_domain_is_on reads exactly this when the domain
    // has a repair_status_mask, which PD_USB does -- BIT(25)).
    const PD_REPAIR_STATUS: usize = 0x290;
    const PD_USB_REPAIR_BIT: u32 = 1 << 25;
    const PD_USB_BIT: u32 = 1 << 11;
    let pmu = dev.res().iomap_region(
        driver_base::MmioRegion::new(PMU_BASE as usize, 0x400),
        "usb-pmu",
    )?;
    if pmu.read32(PD_PWR_CON) & PD_USB_BIT == 0 {
        info!(
            "dwc3: PD_USB already powered ({:#010x} mem {:#010x})",
            pmu.read32(PD_PWR_CON),
            pmu.read32(PD_MEM_PWR),
        );
    } else {
        // Hiword write: write-enable bit 11, clear it.
        pmu.write32(PD_PWR_CON, PD_USB_BIT << 16);
        for _ in 0..10_000 {
            if pmu.read32(PD_PWR_CON) & PD_USB_BIT == 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(1));
        }
        if pmu.read32(PD_PWR_CON) & PD_USB_BIT != 0 {
            log::error!(
                "dwc3: PD_USB stayed powered down (pwr_con {:#010x})",
                pmu.read32(PD_PWR_CON),
            );
            return Err(ProbeError::Fatal(RuntimeError::Io));
        }
    }
    let repair = pmu.read32(PD_REPAIR_STATUS);
    info!(
        "dwc3: PD_USB state pwr_con {:#010x} mem {:#010x} repair {repair:#010x} (on {})",
        pmu.read32(PD_PWR_CON),
        pmu.read32(PD_MEM_PWR),
        repair & PD_USB_REPAIR_BIT != 0,
    );
    enable_vbus(dev)
}

/// Reads `CLKGATE_CON42`, the word this SoC keeps the dwc3 gates in.
///
/// Bits are `SET_TO_DISABLE`: a SET bit means the clock is gated off. A clock
/// whose `prepare_enable()` returns Ok without clearing its bit is the reason
/// this function exists -- the register, not the return value, is the evidence.
fn clock_gate_word(dev: &mut Device<'_, PlatformBus>) -> u32 {
    const CRU_BASE: u64 = 0xfd7c_0000;
    const CLKGATE_CON42: usize = 0x8a8;
    match dev.res().iomap_region(
        driver_base::MmioRegion::new(CRU_BASE as usize, 0x1000),
        "usb-cru-gates",
    ) {
        Ok(cru) => cru.read32(CLKGATE_CON42),
        Err(_) => 0,
    }
}

/// Logs the CRU clock gates the controller depends on.
///
/// "clk enable returned Ok" is not evidence that the gate moved: the register is
/// the evidence. The dwc3 gates live in `CLKGATE_CON42` (0x8a8) in the BSP
/// numbering this board's DT uses -- ids 418..422 are bits 5..9, with
/// `aclk_usb3otg1` (0x1a4) at bit 7, `suspend_clk_usb3otg1` (0x1a5) at bit 8 and
/// `ref_clk_usb3otg1` (0x1a6) at bit 9. Reading `CON26` instead compares two
/// different sets of clocks and says nothing about these.
fn log_cru_clocks(dev: &mut Device<'_, PlatformBus>) {
    const CRU_BASE: u64 = 0xfd7c_0000;
    let Ok(cru) = dev.res().iomap_region(
        driver_base::MmioRegion::new(CRU_BASE as usize, 0x1000),
        "usb-cru",
    ) else {
        return;
    };
    info!(
        "dwc3: cru usb gates con42 {:#010x} (0 = running), softrst_con42 {:#010x}, con25 {:#010x} \
         con26 {:#010x}",
        cru.read32(0x8a8),
        cru.read32(0xaa8),
        cru.read32(0x864),
        cru.read32(0x868),
    );
}

/// Drives the ports' 5V switch, which the firmware models as the
/// `vcc5v0-host` fixed regulator (`gpio = <&gpio3 15 0>`, `regulator-boot-on`)
/// and Linux switches on through the regulator framework. There is no such
/// framework here, so the line is driven directly -- without it the port has no
/// VBUS and no device can ever attach, whatever the controller does.
fn enable_vbus(dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
    // The bank's APB clock is gated at reset (`pclk_gpio3`, table index 129):
    // writes to a GPIO whose pclk is off are dropped and read back as the old
    // value, which is exactly what this driver saw before holding the gate open.
    let gate_guard = enable_cru_clock(dev, PCLK_GPIO3_INDEX);
    const GPIO3_BASE: u64 = 0xfec4_0000;
    const SWPORT_DR: usize = 0x0000;
    const SWPORT_DDR: usize = 0x0008;
    const EXT_PORT: usize = 0x0070;
    const HOST_PORT_BIT: u32 = 1 << 15;
    let gpio = dev.res().iomap_region(
        driver_base::MmioRegion::new(GPIO3_BASE as usize, 0x100),
        "usb-vbus-gpio",
    )?;
    // The pin has to be *muxed* to GPIO before any level can reach the pad.
    // RK3588 keeps the IOMUX of banks >= 1 in the bus IOC block, far from the
    // GPIO registers (Linux: `reg += 0x8000 /* BUS_IOC_BASE */`, and GPIO3 lives
    // in VCCIO3_5 at +0xa000): for GPIO3_B7 the word is 0xfd5fa064, four bits per
    // pin, function 0 = GPIO. This driver never programmed it, so a bank that
    // reads fine could still ignore every level it is handed.
    mux_pin_to_gpio(dev);

    let data = gpio.read32(SWPORT_DR);
    let direction = gpio.read32(SWPORT_DDR);
    info!(
        "dwc3: vbus gpio3.15 dr {data:#010x} ddr {direction:#010x} pad {:#010x}",
        gpio.read32(EXT_PORT),
    );
    if direction & HOST_PORT_BIT != 0 && data & HOST_PORT_BIT != 0 {
        info!("dwc3: vbus already enabled");
        return Ok(());
    }
    // Direction first: driving a level into an input pin does nothing. Each
    // step is verified by read-back and retried: the first attempt after the
    // bank's pclk comes up was observed to vanish, and a level that silently
    // never reached the pad is indistinguishable from "no device attached",
    // which is the worst thing to debug from a console.
    for attempt in 0..4 {
        gpio.write32(SWPORT_DDR, direction | HOST_PORT_BIT);
        gpio.write32(SWPORT_DR, data | HOST_PORT_BIT);
        let now_dr = gpio.read32(SWPORT_DR);
        let now_ddr = gpio.read32(SWPORT_DDR);
        info!(
            "dwc3: vbus attempt {attempt}: dr {now_dr:#010x} ddr {now_ddr:#010x} pad {:#010x}",
            gpio.read32(EXT_PORT),
        );
        if now_dr & HOST_PORT_BIT != 0 && now_ddr & HOST_PORT_BIT != 0 {
            info!("dwc3: vbus on after {} attempt(s)", attempt + 1);
            break;
        }
        for _ in 0..100 {
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        }
    }
    if let Some(guard) = gate_guard {
        // The bank has to stay clocked for the level to survive later writes.
        core::mem::forget(guard);
    }
    Ok(())
}

/// Muxes GPIO3_B7 (the VBUS switch) to the GPIO function.
fn mux_pin_to_gpio(dev: &mut Device<'_, PlatformBus>) {
    const PINCTRL_BASE: u64 = 0xfd5f_0000;
    const VCCIO3_5_IOC: usize = 0xa000;
    const GPIO3_B_MUX: usize = 0x64;
    const PIN_SHIFT: u32 = 28; // B7 is the top nibble of the word
    const PIN_MASK: u32 = 0xf << PIN_SHIFT;
    let Ok(ioc) = dev.res().iomap_region(
        driver_base::MmioRegion::new(PINCTRL_BASE as usize, 0xd000),
        "usb-pinctrl",
    ) else {
        return;
    };
    let offset = VCCIO3_5_IOC + GPIO3_B_MUX;
    let before = ioc.read32(offset);
    if before & PIN_MASK != 0 {
        ioc.write32(offset, before & !PIN_MASK);
        info!(
            "dwc3: gpio3_b7 mux {:#010x} -> {:#010x} (function 0 = gpio)",
            before,
            ioc.read32(offset),
        );
    } else {
        info!("dwc3: gpio3_b7 mux already GPIO ({before:#010x})");
    }
}

/// Takes a DWC3 instance's USB3 port out of reset in the USB GRF.
///
/// `rockchip,rk3588-usb-grf` (the DT's `rockchip,usbctrl-grf`, syscon at
/// 0xfd5ac000) holds one `USB3OTGn_CFG` word per controller: 0x1c for port 0
/// (`usb@fc000000`) and 0x34 for port 1 (`usb@fc400000`). The write is the
/// Rockchip hiword-mask form with data `0x8800`.
fn enable_u3_port(dev: &mut Device<'_, PlatformBus>, controller_base: usize) {
    const USB_GRF: u64 = 0xfd5a_c000;
    const GRF_WINDOW: usize = 0x4000;
    let port = if controller_base < 0xfc40_0000 { 0 } else { 1 };
    let offset = if port == 0 { 0x1c } else { 0x34 };
    let Ok(grf) = dev.res().iomap_region(
        driver_base::MmioRegion::new(USB_GRF as usize, GRF_WINDOW),
        "usb-grf",
    ) else {
        return;
    };
    let before = grf.read32(offset);
    // Bits [31:16] are the write-enable mask; [15:0] the data:
    // bit15 PIPE_ENABLE=1, bit12 PHY_DISABLE=0, bit10 SUSPEND_ENABLE=0,
    // bit8 U3_PORT_DISABLE=0.
    grf.write32(offset, 0xffff_8800);
    info!(
        "dwc3: usb-grf u3 port {port} cfg {before:#06x} -> {:#06x}",
        grf.read32(offset),
    );
}

/// Reads one CRU word (read-only diagnostic helper).
fn cru_word(dev: &mut Device<'_, PlatformBus>, offset: usize) -> u32 {
    const CRU_BASE: u64 = 0xfd7c_0000;
    match dev.res().iomap_region(
        driver_base::MmioRegion::new(CRU_BASE as usize, 0x1000),
        "usb-cru-word",
    ) {
        Ok(cru) => cru.read32(offset),
        Err(_) => 0,
    }
}

/// Table index of `pclk_gpio3` in the CRU branch table.
const PCLK_GPIO3_INDEX: u32 = 129;

/// Enables a CRU clock by table index, through the same provider the node's
/// parent lists (so no phandle is hard-coded).
fn enable_cru_clock(dev: &mut Device<'_, PlatformBus>, index: u32) -> Option<EnabledClock> {
    let provider = parent_clock_provider(dev)?;
    let spec = ClkSpec::by_phandle_index(provider, index);
    let clock = dev.res().clk_spec(spec).ok()?;
    // The register, not the return value: `CLKGATE_CON17` (0x844) bit 2 is
    // `pclk_gpio3`, and a gate bit still set means the bank is still gated --
    // which would explain writes that read back unchanged.
    let before = cru_word(dev, 0x844);
    let guard = clock.prepare_enable().ok()?;
    info!(
        "dwc3: clock index {index} at {} Hz, con17 {before:#010x} -> {:#010x}",
        clock.get_rate(),
        cru_word(dev, 0x844),
    );
    Some(guard)
}

/// The clock provider phandle the controller's parent node lists.
fn parent_clock_provider(dev: &Device<'_, PlatformBus>) -> Option<u64> {
    let phandle = {
        let fw = dev.fw();
        fw.node()?.phandle?
    };
    let node = of::find_node_by_phandle(phandle)?;
    let raw = node.parent_property("clocks")?.value;
    let cells: Vec<u32> = raw
        .chunks_exact(4)
        .map(|cell| u32::from_be_bytes(cell.try_into().unwrap_or([0; 4])))
        .collect();
    Some(u64::from(*cells.first()?))
}

/// Brings both EHCI controllers' ports up far enough to report a connection.
///
/// This port does not drive EHCI yet, but the two USB2 Type-A ports belong to
/// it: reading their `PORTSC` before port power is switched on returns zero and
/// says nothing, which made "the device is plugged into a USB2 port" and "there
/// is no device at all" indistinguishable. Setting port power (and running the
/// controller) turns the question into a readable bit.
fn probe_ehci_ports(dev: &mut Device<'_, PlatformBus>) {
    const EHCI: [(u64, &str); 2] = [(0xfc80_0000, "fc800000"), (0xfc88_0000, "fc880000")];
    const USBCMD: usize = 0x00;
    const USBSTS: usize = 0x04;
    const CONFIGFLAG: usize = 0x40;
    const PORTSC0: usize = 0x44;
    for (base, name) in EHCI {
        let Ok(ehci) = dev.res().iomap_region(
            driver_base::MmioRegion::new(base as usize, 0x1000),
            "usb-ehci",
        ) else {
            continue;
        };
        let caplen = ehci.read8(0) as usize;
        // Host controller reset, then run.
        ehci.write32(caplen + USBCMD, 1 << 1);
        for _ in 0..10_000 {
            if ehci.read32(caplen + USBCMD) & (1 << 1) == 0 {
                break;
            }
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(10));
        }
        ehci.write32(caplen + CONFIGFLAG, 1);
        ehci.write32(caplen + USBCMD, 1);
        let ports = ehci.read32(0x04) & 0xf;
        let status = ehci.read32(caplen + USBSTS);
        khal::time::busy_wait(ktime_types::TimeSpan::from_micros(50_000));
        for port in 0..ports as usize {
            let offset = caplen + PORTSC0 + port * 4;
            let before = ehci.read32(offset);
            // Port power (bit 12) has to be on before a connect can be seen.
            ehci.write32(offset, before | (1 << 12));
            khal::time::busy_wait(ktime_types::TimeSpan::from_micros(50_000));
            let after = ehci.read32(offset);
            info!(
                "dwc3: ehci {name} port {} portsc {after:#010x} ccs {} pp {} (usbsts \
                 {status:#010x})",
                port + 1,
                after & 1 != 0,
                after & (1 << 12) != 0,
            );
        }
    }
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
