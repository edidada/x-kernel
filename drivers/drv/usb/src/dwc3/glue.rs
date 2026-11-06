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
use rockchip_pmu;

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
    ///
    /// Shared because the mass-storage block backend reads through it from
    /// `&self`; the publications below must outlive the probe for the same reason
    /// the EHCI side keeps them.
    _xhci: alloc::sync::Arc<ksync::Mutex<crate::xhci::Xhci>>,
    /// The device table: devices, their class bindings, and the withdrawal owners
    /// of everything published for them. Kept here so it outlives probe.
    _bus: alloc::sync::Arc<ksync::Mutex<usb::bus::UsbBus>>,
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

        // MaxPorts (HCSPARAMS1 bits 31:24) is bracketed across our own init:
        // here (clocks on, PHYs untouched), after the USBDP PHY init, and after
        // the core soft reset. The working board reads 2 at runtime while this
        // port ends up with 1, with GHWPARAMS3, GCTL and GUSB3PIPECTL all
        // bit-identical — so one of our steps changes it. It cannot be read any
        // earlier than this: the xHCI window needs its bus clock, and reading it
        // before the clock loop stalls the interconnect outright (the console
        // just stops, with no abort).
        info!(
            "dwc3: HCSPARAMS1 after clocks {:#010x} (max ports {})",
            regs.read32(0x04),
            (regs.read32(0x04) >> 24) & 0xff,
        );

        // Select the host role while the PHYs are still down. The peripheral ->
        // host transition is what freezes this core's port map, and it costs the
        // SuperSpeed port if the SS PIPE is already running at that moment
        // (measured: MaxPorts 2 before the transition, 1 after, with GCTL
        // bit-identical to the working board). `core::init` repeats the same
        // write later — a same-value write does not re-latch.
        crate::dwc3::core::select_host_role(&regs);
        info!(
            "dwc3: HCSPARAMS1 after early host role {:#010x} (max ports {})",
            regs.read32(0x04),
            (regs.read32(0x04) >> 24) & 0xff,
        );

        // USB2 PHY only in this stage: High-Speed works through it, and the
        // SuperSpeed PHY is a separate driver that lands later.
        let mut phys = Vec::new();
        let usb2 = usb2_phy_of_port(dev)?;
        phys.push(
            usb2.init_powered()
                .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?,
        );
        info!("dwc3: usb2 phy powered");
        // The USB3 (USBDP) PHY too: it owns the OTG port's U3/480M path, and
        // its init runs only once a consumer acquires the published u3-port.
        let usb3 = usb3_phy_of_port(dev)?;
        phys.push(
            usb3.init_powered()
                .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?,
        );
        info!("dwc3: usb3 phy powered");
        // Diagnostic: +0x0000 bit0 is the reference's `clkout_ctl` (480 MHz
        // output gate), +0x0c bit11 is `phy_sus`. Both settle whether the
        // PHY is actually feeding the controller.
        log_usb2phy_state(dev);

        // The USB3 port's GRF word is owned by the USBDP PHY provider (the
        // reference enables it in `rk_udphy_usb3_phy_init`); this only logs it.
        log_u3_port(dev, regs.region().base);

        let utmi_wide = dev.fw().string("phy_type") == Some("utmi_wide");
        let dis_enblslpm = dev.fw().flag("snps,dis_enblslpm_quirk");
        info!(
            "dwc3: HCSPARAMS1 after PHY init {:#010x} (max ports {})",
            regs.read32(0x04),
            (regs.read32(0x04) >> 24) & 0xff,
        );
        let _params = crate::dwc3::core::init(dev, &regs, utmi_wide, dis_enblslpm)?;
        info!(
            "dwc3: HCSPARAMS1 after core init {:#010x} (max ports {})",
            regs.read32(0x04),
            (regs.read32(0x04) >> 24) & 0xff,
        );

        // The host controller takes the window over: the xHCI registers sit at
        // offset 0 of the same mapping the core's global registers live above.
        let mut xhci = crate::xhci::Xhci::init(dev, regs)?;
        xhci.log_ports();
        // Enumerate what is already attached. The controller runs on its own
        // after this, so a device that appears later needs interrupt delivery,
        // which this port does not have yet. PHY state at the moment the
        // controller first has to talk to the device:
        log_usb2phy_state(dev);
        // The controller becomes shareable here: a class driver bind and the
        // block backend both need it through a shared handle, while enumeration
        // needs it mutably.
        let xhci = alloc::sync::Arc::new(ksync::Mutex::new(xhci));
        let (found, ready, devices) = {
            let mut guard = xhci.lock();
            guard.enumerate(dev)
        };
        info!("dwc3: enumeration found {found} device(s), {ready} usable");

        // The device table: created here because this is the only place a
        // `DeviceHandle` exists, owned by the controller afterwards so a port scan
        // at runtime can withdraw and publish bindings without a `Device`.
        let bus = alloc::sync::Arc::new(ksync::Mutex::new(usb::bus::UsbBus::new(dev.handle())));
        for device in &devices {
            {
                let mut guard = xhci.lock();
                crate::xhci::probe_mass_storage(&mut *guard, device);
            }
            if let Err(err) = bus.lock().attach(device.clone()) {
                log::error!("dwc3: attaching {} failed: {err:?}", device.summary());
                continue;
            }
            let publication = {
                let handle = bus.lock().handle().clone();
                crate::xhci::publish_mass_storage(&handle, &xhci, device)
            };
            match publication {
                Ok(publication) => {
                    let pipe_interface = device
                        .configurations
                        .first()
                        .and_then(|config| config.interfaces.first())
                        .map_or(0, |interface| interface.number);
                    if let Err(err) = bus.lock().bind(
                        device.address,
                        usb::bus::ClassBinding::new(
                            "usb-storage",
                            pipe_interface,
                            alloc::vec![alloc::boxed::Box::new(publication)],
                        ),
                    ) {
                        log::error!("dwc3: binding {} failed: {err:?}", device.summary());
                    }
                }
                Err(err) => log::error!("dwc3: no class binding for {}: {err:?}", device.summary()),
            }
        }
        {
            let mut guard = xhci.lock();
            guard.attach_bus(bus.clone());
            guard.attach_self(&xhci);
        }

        Ok(Dwc3Data {
            _clocks: clocks,
            _clock_guards: clock_guards,
            _reset: reset,
            _phys: phys,
            _xhci: xhci,
            _bus: bus,
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
    // `rockchip_pd_power()` semantics through the shared PMU module: the PMU
    // mutex serialises the two dwc3 controllers and the PCIe driver, and a
    // domain already in the requested state is a no-op.
    rockchip_pmu::attach_via(dev).map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
    let Some((pwr_con, mem_pwr, repair)) = rockchip_pmu::state_words() else {
        return Err(ProbeError::Fatal(RuntimeError::Io));
    };
    info!(
        "dwc3: PD_USB state pwr_con {pwr_con:#010x} mem {mem_pwr:#010x} repair {repair:#010x} (on \
         {})",
        repair & rockchip_pmu::PD_USB_REPAIR_BIT != 0,
    );
    rockchip_pmu::power_domain_on(
        "usb",
        rockchip_pmu::PD_USB_BIT,
        rockchip_pmu::PD_USB_REPAIR_BIT,
    )
    .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
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
    // value.
    let gate_guard = enable_cru_clock(dev, PCLK_GPIO3_INDEX);
    const GPIO3_BASE: u64 = 0xfec4_0000;
    const SWPORT_DR_L: usize = 0x0000;
    const SWPORT_DR_H: usize = 0x0004;
    const SWPORT_DDR_L: usize = 0x0008;
    const SWPORT_DDR_H: usize = 0x000c;
    const EXT_PORT_L: usize = 0x0070;
    const HOST_PORT_BIT: u32 = 15;
    let gpio = dev.res().iomap_region(
        driver_base::MmioRegion::new(GPIO3_BASE as usize, 0x100),
        "usb-vbus-gpio",
    )?;

    // RK3588 is a "GPIO v2" bank: the data and direction registers are split
    // into two halves, and a half is written with the affected bits marked in
    // the HIGH half of the word -- the same convention as the CRU gates.
    // Writing a plain value leaves the mask at zero and the hardware updates
    // nothing at all, which is exactly how this driver spent several rounds
    // believing the bank refused writes: the register read back unchanged
    // because not one bit had been enabled for writing.
    let write_bit = |low: usize, high: usize, bit: u32, set: bool| {
        let mask = 1u32 << bit;
        let data = if set { mask | (mask << 16) } else { mask << 16 };
        let offset = if bit < 16 { low } else { high };
        gpio.write32(offset, data);
    };
    let read_bit = |low: usize, high: usize, bit: u32| {
        let value = gpio.read32(low) | (gpio.read32(high) << 16);
        value & (1u32 << bit) != 0
    };

    // The pin must be muxed to GPIO before a level can reach the pad.
    mux_pin_to_gpio(dev);

    info!(
        "dwc3: vbus gpio3.15 ddr {} dr {} pad {}",
        read_bit(SWPORT_DDR_L, SWPORT_DDR_H, HOST_PORT_BIT),
        read_bit(SWPORT_DR_L, SWPORT_DR_H, HOST_PORT_BIT),
        read_bit(EXT_PORT_L, EXT_PORT_L + 4, HOST_PORT_BIT),
    );
    // Direction first: driving a level into an input pin does nothing.
    write_bit(SWPORT_DDR_L, SWPORT_DDR_H, HOST_PORT_BIT, true);
    write_bit(SWPORT_DR_L, SWPORT_DR_H, HOST_PORT_BIT, true);
    let driven = read_bit(SWPORT_DR_L, SWPORT_DR_H, HOST_PORT_BIT)
        && read_bit(SWPORT_DDR_L, SWPORT_DDR_H, HOST_PORT_BIT);
    if driven {
        info!("dwc3: vbus on (ddr/dr bit15 set)");
    } else {
        log::error!("dwc3: vbus switch did not take (gpio3.15 still low)");
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
/// One-line-per-block dump of the two usb2phy-grf OTG blocks' key words:
/// `+0x0000` bit0 = `clkout_ctl` (the PHY's 480 MHz output gate) and
/// `+0x0c` bit11 = `phy_sus` (suspended when 1). These are the two signals
/// that decide whether D+ is detectable and whether transactions complete.
fn log_usb2phy_state(dev: &mut Device<'_, PlatformBus>) {
    for (base, name) in [(0xfd5d_0000_u64, "usb3_0"), (0xfd5d_4000, "usb3_1")] {
        if let Ok(block) = dev.res().iomap_region(
            driver_base::MmioRegion::new(base as usize, 0x4000),
            "usb2phy-state",
        ) {
            let word0 = block.read32(0x0000);
            let word_c = block.read32(0x000c);
            info!(
                "dwc3: usb2phy {name} +0x00 {word0:#06x} (clkout_ctl {}) +0x0c {word_c:#06x} \
                 (suspended {})",
                if word0 & 0x1 != 0 { "on" } else { "off" },
                word_c & (1 << 11) != 0,
            );
        }
    }
}

/// Reads (never writes) the controller's U3 port configuration word.
///
/// This word belongs to the USBDP PHY provider: the reference's
/// `rk_udphy_usb3_phy_init` calls `udphy_u3_port_disable(false)`, which
/// writes `0x0188` (PIPE enable + phy/suspend/port-disable cleared) from
/// `phy-rockchip-usbdp.c`. The glue used to write `0x1100` here — the
/// reference's *disable* value — right after the PHY had enabled the port,
/// so the controller was handed a port the PHY had just switched back off.
/// Writing it from two owners was the bug; this is now observation only.
fn log_u3_port(dev: &mut Device<'_, PlatformBus>, controller_base: usize) {
    /// `rk3588_udphy_cfgs.grfcfg.usb3otg{0,1}_cfg.enable`.
    const US3OTG_ENABLE: u32 = 0x0188;
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
    let cfg = grf.read32(offset);
    info!(
        "dwc3: usb-grf u3 port {port} cfg {cfg:#06x} ({} the reference's enable word {:#06x})",
        if cfg & 0xffff == US3OTG_ENABLE {
            "matches"
        } else {
            "differs from"
        },
        US3OTG_ENABLE,
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

/// Acquires the USB3 (SuperSpeed / USBDP) PHY the `usb3-phy` entry points at.
///
/// Unlike the usb2-phy (whose firmware references the port node and whose
/// provider binds the parent block), the usbdp provider publishes the
/// `u3-port` node directly, so no port→block re-resolution is needed — the
/// spec's phandle IS the published one. Without this PHY the DWC3 OTG port's
/// SuperSpeed/480M path is never powered, which is exactly how a port that
/// detects a connect but times out on SET_ADDRESS looks.
fn usb3_phy_of_port(dev: &Device<'_, PlatformBus>) -> Result<driver_base::PhyHandle, ProbeError> {
    let spec = dev
        .res()
        .map()
        .phy_by_name("usb3-phy")
        .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
    info!("dwc3: usb3 phy port {:#x}", spec.provider);
    dev.res().phy_spec(spec)
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
