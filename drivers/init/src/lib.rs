// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Composition root for the driver-core device model.
//!
//! Called once from the runtime init (`kruntime`):
//!
//! 1. installs the x-kernel service runtime and wires it to the global
//!    device domain (deferred probes wake through it);
//! 2. registers the compiled-in drivers (`module_init` equivalents, one
//!    line each, gated on cargo features);
//! 3. enumerates the firmware device tree into the domain as
//!    [`DeviceDescriptor`]s with named resources (`reg`, `interrupts`,
//!    `clocks` + `clock-names`, provider attributes).
//!
//! The domain takes it from there: consumers that probe before their clock
//! providers defer and are woken exactly when the provider publishes — no
//! ordering table, no retry loop.

#![no_std]

extern crate alloc;

use core::sync::atomic::{AtomicU32, Ordering};

use driver_base::{
    Bus, DmaInfo, FirmwareNodeInfo, IrqTrigger, PlatformBus, PlatformBusData, ResourceKind,
    ResourceMap, desc::DeviceDescriptor, global_domain,
};
use khal::firmware::devices as firmware;

/// Bring up the new device model.
pub fn init() {
    // Initcall levels, the Linux do_initcalls() shape — each level may
    // rely on everything below it being done:
    //
    //   L1  host services    — the resource provider the model fronts
    //   L2  subsystem cores  — per-class cores (block reclaimer, ...)
    //   L3  drivers          — device drivers, one line each
    //   L4  enumeration      — firmware/static descriptors enter the domain
    //   L5  probe storm      — match/bind/defer; deferred probes wake as
    //                          providers (clocks, buses) register
    //
    // Cargo features select WHO is compiled in; this ordering is WHAT
    // runs in which order — the two axes are independent now.
    level1_host_services();
    level2_subsystem_cores();
    level3_drivers();
    level4_enumeration();
    level5_probe_storm();

    // Seed the wall clock after the probe storm: RTCs are ordinary platform
    // devices now, so this is the model's `late_initcall(rtc_hctosys)`.
    // `ktime::initialize_realtime` is idempotent, so a platform that
    // supplied an earlier sample keeps it.
    #[cfg(feature = "rtc")]
    rtc_hctosys();
}

/// Seed the kernel wall clock from the RTC class (Linux `rtc_hctosys`).
#[cfg(feature = "rtc")]
fn rtc_hctosys() {
    if subsys_rtc::hctosys().is_none() {
        log::warn!("driver-core: no RTC device supplied the boot wall clock");
    }
}

fn level1_host_services() {
    driver_host::Host::install_global();
}

fn level2_subsystem_cores() {
    // The block reclaimer is the one subsystem core that needs explicit
    // bring-up before any block driver registers.
    #[cfg(any(feature = "virtio-blk", feature = "ramdisk"))]
    subsys_block_host::reclaimer::init();
}

fn level3_drivers() {
    #[cfg(feature = "fixed-clock")]
    drv_clk::fixed_clock::register();
    #[cfg(feature = "fixed-factor")]
    drv_clk::fixed_factor::register();
    #[cfg(feature = "clk-scmi")]
    drv_clk::scmi_clock::register();
    #[cfg(feature = "rk3588-cru")]
    drv_clk::rk3588_cru::register();
    #[cfg(feature = "rk3588-usb2phy")]
    drv_phy::usb2phy::register();
    #[cfg(feature = "rk3588-combphy")]
    drv_phy::combphy::register();
    #[cfg(feature = "rk3588-pcie3phy")]
    drv_phy::pcie3phy::register();
    #[cfg(feature = "pci-dw-host")]
    pci::dw_host::register();
    #[cfg(feature = "net-rtl8125")]
    drv_net::rtl8125::register();
    #[cfg(feature = "usb-dwc3")]
    drv_usb::register();
    #[cfg(feature = "mmc-block")]
    subsys_mmc::blk::register();
    #[cfg(feature = "mmc-dwmmc-rockchip")]
    drv_mmc::dw_mmc_rockchip::register();
    #[cfg(feature = "serial-pl011")]
    drv_serial::pl011::register();
    #[cfg(feature = "virtio-blk")]
    drv_virtio::blk::register();
    #[cfg(feature = "virtio-net")]
    drv_virtio::net::register();
    #[cfg(feature = "virtio-rng")]
    drv_virtio::rng::register();
    #[cfg(feature = "virtio-gpu")]
    drv_virtio::gpu::register();
    #[cfg(feature = "virtio-input")]
    drv_virtio::input::register();
    #[cfg(feature = "virtio-vsock")]
    drv_virtio::vsock::register();
    #[cfg(feature = "virtio-9p")]
    drv_virtio::virtio_9p::register();
    #[cfg(feature = "serial-ns16550-mmio")]
    drv_serial::ns16550::register_mmio();
    #[cfg(feature = "serial-ns16550-ioport")]
    drv_serial::ns16550::register_ioport();
    #[cfg(feature = "rtc-pl031")]
    drv_rtc::pl031::register();
    #[cfg(feature = "rtc-goldfish")]
    drv_rtc::goldfish::register();
    #[cfg(feature = "rtc-cmos")]
    drv_rtc::cmos::register();
    #[cfg(feature = "rtc-ls7a")]
    drv_rtc::ls7a::register();
    #[cfg(feature = "virtio-mmio")]
    drv_virtio::mmio::register();
    #[cfg(feature = "ramdisk")]
    drv_block::ramdisk::register();
}

fn level4_enumeration() {
    enumerate_firmware_devices();
    enumerate_static_devices();
    #[cfg(any(
        feature = "virtio-blk",
        feature = "virtio-net",
        feature = "virtio-rng",
        feature = "virtio-gpu",
        feature = "virtio-input",
        feature = "virtio-vsock",
        feature = "virtio-9p",
        feature = "net-rtl8125"
    ))]
    pci_discover();
}

fn level5_probe_storm() {
    let domain = global_domain();
    let devices = domain.devices();
    let active = devices
        .iter()
        .filter(|snapshot| snapshot.state == driver_base::DeviceState::Active)
        .count();
    log::info!(
        "driver-core: {} firmware descriptor(s), {active} active",
        devices.len()
    );

    // Probe-phase end: every driver and provider for this build registered.
    // Devices still deferred are stuck — report and convert to Unclaimed
    // with a warning instead of parking them silently forever.
    let stuck = domain.deferred_report();
    if !stuck.is_empty() {
        for (id, dep) in &stuck {
            log::warn!("driver-core: device {id} still deferred on {dep} at boot end");
        }
        domain.finish_probe_phase(false);
    }
}

/// Translate the firmware device tree into domain descriptors.
///
/// Every firmware node becomes a descriptor; nodes without a matching driver
/// end up `Unclaimed` (visible in introspection, harmless). This is the
/// enumeration path for the device domain.
fn enumerate_firmware_devices() {
    // The khal `for_each_compatible` matcher must echo a `&'static str` (it
    // selects the firmware id from driver tables); enumerating *every* node
    // instead walks the tree directly. The DT blob is 'static, so taking the
    // node's first compatible as the identity is sound.
    enumerate_firmware_nodes_direct();
}

/// Monotonic firmware-node location id, allocated per boot in enumeration
/// order (stable within one boot). Every DT node gets a distinct
/// `DeviceLocation::FirmwareNode`, so devnode paths, uevent DEVPATH and
/// class parentage stay unique per device.
///
/// u32 with an exhaustion guard: a wrap would silently hand two devices
/// the same location, so the allocator refuses (and logs once) instead —
/// a 4-billion-node device tree is a firmware error, not a case to mask.
static FIRMWARE_NODE_ID: AtomicU32 = AtomicU32::new(0);

fn next_firmware_node_id() -> Option<u32> {
    let id = FIRMWARE_NODE_ID.fetch_add(1, Ordering::Relaxed);
    if id == u32::MAX {
        log::error!("firmware node id space exhausted; skipping further DT devices");
        return None;
    }
    Some(id)
}

fn enumerate_firmware_nodes_direct() {
    let Some(fdt) = of::fdt() else {
        return;
    };
    for node in fdt.all_nodes() {
        // Naked clock-provider nodes (the SCMI `protocol@14` shape: no
        // compatible, but a `#clock-cells` and a phandle consumers bind
        // to) are enumeration products all the same — synthesize the
        // generic "clock-provider" identity so a provider driver can
        // claim them. Everything else without a compatible is skipped.
        let mut all_compatibles: alloc::vec::Vec<&'static str> = node.compatibles().collect();
        if all_compatibles.is_empty() {
            if node.property_u32("#clock-cells").is_none() {
                continue;
            }
            all_compatibles.push("clock-provider");
        }
        let Some(firmware_id) = all_compatibles.first().copied() else {
            continue;
        };
        let dev = firmware::FirmwareDevice {
            firmware_id,
            all_compatibles,
            source: firmware::FirmwareSource::DeviceTree,
            mmios: {
                let reg_names = of::string_list(node, "reg-names");
                node.reg()
                    .map(|regs| {
                        regs.enumerate()
                            .map(|(idx, reg)| firmware::MmioResource {
                                base: reg.starting_address as usize,
                                size: reg.size,
                                name: reg_names.as_ref().and_then(|names| names.get(idx)).copied(),
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            },
            irqs: of::interrupt_descs(node),
            irq_names: of::string_list(node, "interrupt-names"),
            clock_frequency: node.property_u32("clock-frequency"),
            phandle: node.property_u32("phandle"),
            clock_cells: node.property_u32("#clock-cells"),
            clock_output_name: of::clock_output_name(node),
            clock_specs: of::device_clock_specs(node),
            reset_specs: of::device_reset_specs(node),
            phy_specs: of::device_phy_specs(node),
            props: of::node_properties(node),
            dma: of::device_dma_facts(node),
        };
        build_descriptor(dev);
    }
}

/// Combine firmware-stated DMA facts into the model's DMA view.
///
/// Firmware authority: a DT node with NO DMA facts at all is NOT silently
/// given the static-device default — firmware had its chance to speak and
/// said nothing about coherency, so the view is strict identity,
/// full-mask, NON-coherent (the arch64 Linux reading of a missing
/// `dma-coherent`); the host's cache-maintenance protocol is then on and
/// is a no-op only on hosts whose memory is physically coherent. Only
/// facts the firmware actually stated (`dma-ranges` rows) override the
/// identity translation.
fn dma_info_from_facts(facts: Option<firmware::DmaFacts>) -> DmaInfo {
    match facts {
        Some(facts) => DmaInfo {
            mask: facts.size_mask,
            offset: facts.offset,
            coherent: facts.coherent.unwrap_or(false),
        },
        None => DmaInfo {
            mask: u64::MAX,
            offset: 0,
            coherent: false,
        },
    }
}

fn build_descriptor(dev: firmware::FirmwareDevice) {
    let mut resources = ResourceMap::new();
    for mmio in &dev.mmios {
        let kind = ResourceKind::Mmio(driver_base::MmioRegion::new(mmio.base, mmio.size));
        match mmio.name {
            Some(name) => resources.push_named(name, kind),
            None => resources.push_kind(kind),
        }
    }
    for (idx, irq) in dev.irqs.iter().enumerate() {
        // Keep the firmware route intact: controller + controller-relative
        // hwirq travel with the resource, and the runtime maps them through
        // the controller's irq domain at request time (Linux keeps DT spec
        // cells until irq_create_of_mapping). Compressing to a bare global
        // number only worked where the domain map is the identity (QEMU).
        // A firmware route: the DT number IS the controller-relative
        // hwirq (GIC 32+SPI math applied by the parser), not an
        // OS-visible virq — resolution happens through the controller's
        // irq domain at request time.
        let controller = match irq.controller {
            firmware::InterruptControllerKind::Gic => driver_base::IrqController::Gic,
            firmware::InterruptControllerKind::Plic => driver_base::IrqController::Plic,
            firmware::InterruptControllerKind::Unknown => driver_base::IrqController::Unknown,
        };
        let spec = driver_base::IrqSpec::firmware(controller, irq.irq, map_trigger(irq.trigger));
        let name = dev
            .irq_names
            .as_ref()
            .and_then(|names| names.get(idx))
            .copied();
        match name {
            Some(name) => resources.push_named(name, ResourceKind::Irq(spec)),
            None => resources.push_kind(ResourceKind::Irq(spec)),
        }
    }
    for spec in &dev.clock_specs {
        let entry_kind = ResourceKind::Clk(driver_base::ClkSpec::by_phandle_index(
            spec.provider as u64,
            spec.index,
        ));
        match spec.connection_name {
            Some(name) => resources.push_named(name, entry_kind),
            None => resources.push_kind(entry_kind),
        }
    }
    for spec in &dev.reset_specs {
        let entry_kind = ResourceKind::Reset(driver_base::ResetSpec::by_phandle_index(
            u64::from(spec.provider),
            spec.index,
        ));
        match spec.connection_name {
            Some(name) => resources.push_named(name, entry_kind),
            None => resources.push_kind(entry_kind),
        }
    }
    for spec in &dev.phy_specs {
        let entry_kind = ResourceKind::Phy(driver_base::PhySpec::by_phandle_cells(
            u64::from(spec.provider),
            spec.cells,
            spec.ncells,
        ));
        match spec.connection_name {
            Some(name) => resources.push_named(name, entry_kind),
            None => resources.push_kind(entry_kind),
        }
    }

    let firmware_info = FirmwareNodeInfo {
        phandle: dev.phandle,
        clock_frequency: dev.clock_frequency,
        clock_cells: dev.clock_cells,
        clock_output_name: dev.clock_output_name,
        properties: dev.props.clone(),
    };

    let Some(node_id) = next_firmware_node_id() else {
        return;
    };
    // A firmware-described device ALWAYS carries its node snapshot —
    // the property bag is the quirk surface, not a provider-only extra.
    let mut data = PlatformBusData::firmware_node(node_id, &dev.all_compatibles);
    // The full compatible list: matching iterates it (of_device_id
    // any-match semantics); modalias reports the primary entry.
    data.identity.firmware_id = Some(alloc::string::String::from(dev.firmware_id));
    let descriptor = DeviceDescriptor::new(<PlatformBus as Bus>::ID, alloc::boxed::Box::new(data))
        .with_resources(resources)
        .with_firmware(Some(firmware_info))
        .with_dma(dma_info_from_facts(dev.dma));

    match global_domain().add_device(descriptor) {
        Ok(added) => log::debug!("driver-core: enumerated {added:?}"),
        Err(err) => log::warn!("driver-core: device enumeration rejected: {err:?}"),
    }
}

/// x86 ISA COM port bases (board-static: no firmware description).
#[cfg(feature = "serial-ns16550-ioport")]
static COM_PORTS: [u16; 4] = [0x3f8, 0x2f8, 0x3e8, 0x2e8];

/// x86 CMOS clock command port; the data port follows it.
#[cfg(feature = "rtc-cmos")]
const RTC_CMOS_PORT: u16 = 0x70;

/// LS7A RTC aperture size (the `RTC_PADDR` window).
#[cfg(feature = "rtc-ls7a")]
const RTC_LS7A_SIZE: usize = 0x1000;

/// Board-static devices (no firmware description): the compose layer is
/// the only registrar since the legacy backend went away — the ramdisk,
/// the derived clock, the x86 COM ports and the board-static RTCs all live
/// here.
fn enumerate_static_devices() {
    #[cfg(feature = "ramdisk")]
    {
        // Board-static devices carry no DT compatible list; drivers
        // claim them by alias, not by of-match.
        let descriptor = DeviceDescriptor::new(
            <PlatformBus as Bus>::ID,
            alloc::boxed::Box::new(PlatformBusData::static_config(0, "ramdisk")),
        );
        match global_domain().add_device(descriptor) {
            Ok(added) => log::debug!("driver-core: enumerated {added:?}"),
            Err(err) => log::warn!("driver-core: device enumeration rejected: {err:?}"),
        }
    }

    // Derived clock (transcription exercise): board-static because QEMU's DT
    // has no node for it. The parent edge is a real firmware reference —
    // `clocks = <&apb_pclk 0>` — resolved by `dev.res().clk("parent")`.
    #[cfg(feature = "fixed-factor")]
    {
        let mut resources = ResourceMap::new();
        resources.push_named(
            "parent",
            ResourceKind::Clk(driver_base::ClkSpec::by_phandle_index(0x8000, 0)),
        );
        let descriptor = DeviceDescriptor::new(
            <PlatformBus as Bus>::ID,
            alloc::boxed::Box::new(PlatformBusData::static_config(2, "clk48m")),
        )
        .with_resources(resources);
        match global_domain().add_device(descriptor) {
            Ok(added) => log::debug!("driver-core: enumerated {added:?}"),
            Err(err) => log::warn!("driver-core: device enumeration rejected: {err:?}"),
        }
    }

    // The x86 CMOS clock has no firmware description: the board-static
    // descriptor states the architecture-defined command/data port pair,
    // and the driver refuses to run if the hardware model disagrees with
    // the board description.
    #[cfg(feature = "rtc-cmos")]
    {
        let mut resources = ResourceMap::new();
        resources.push_kind(ResourceKind::IoPort(driver_base::IoPortRange {
            base: RTC_CMOS_PORT,
            size: 2,
        }));
        let descriptor = DeviceDescriptor::new(
            <PlatformBus as Bus>::ID,
            alloc::boxed::Box::new(PlatformBusData::static_config(0x10, "rtc-cmos")),
        )
        .with_resources(resources);
        match global_domain().add_device(descriptor) {
            Ok(added) => log::debug!("driver-core: enumerated {added:?}"),
            Err(err) => log::warn!("driver-core: device enumeration rejected: {err:?}"),
        }
    }

    // Boards that describe the LS7A RTC only by the board-static aperture
    // (the `RTC_PADDR` build parameter) instead of a device-tree node.
    #[cfg(feature = "rtc-ls7a")]
    {
        if kbuild_config::RTC_PADDR == 0 {
            log::warn!("driver-core: rtc-ls7a enabled but RTC_PADDR is unset; skipping");
        } else {
            let mut resources = ResourceMap::new();
            resources.push_kind(ResourceKind::Mmio(driver_base::MmioRegion::new(
                kbuild_config::RTC_PADDR as usize,
                RTC_LS7A_SIZE,
            )));
            let descriptor = DeviceDescriptor::new(
                <PlatformBus as Bus>::ID,
                alloc::boxed::Box::new(PlatformBusData::static_config(0x11, "rtc-ls7a")),
            )
            .with_resources(resources);
            match global_domain().add_device(descriptor) {
                Ok(added) => log::debug!("driver-core: enumerated {added:?}"),
                Err(err) => log::warn!("driver-core: device enumeration rejected: {err:?}"),
            }
        }
    }

    // Each ISA COM port becomes a platform-static device carrying its I/O
    // port range; Ns16550IoPortDriver claims them by alias and reads the
    // range from the resource table.
    #[cfg(feature = "serial-ns16550-ioport")]
    for (idx, base) in COM_PORTS.iter().enumerate() {
        let mut resources = ResourceMap::new();
        resources.push_kind(ResourceKind::IoPort(driver_base::IoPortRange {
            base: *base,
            size: 8,
        }));
        let descriptor = DeviceDescriptor::new(
            <PlatformBus as Bus>::ID,
            alloc::boxed::Box::new(PlatformBusData::static_config(
                1 + idx as u16,
                "serial-ns16550-ioport",
            )),
        )
        .with_resources(resources);
        match global_domain().add_device(descriptor) {
            Ok(added) => log::debug!("driver-core: enumerated {added:?}"),
            Err(err) => log::warn!("driver-core: device enumeration rejected: {err:?}"),
        }
    }
}

fn map_trigger(trigger: firmware::InterruptTrigger) -> IrqTrigger {
    match trigger {
        firmware::InterruptTrigger::EdgeRising => IrqTrigger::EdgeRising,
        firmware::InterruptTrigger::EdgeFalling => IrqTrigger::EdgeFalling,
        firmware::InterruptTrigger::LevelHigh => IrqTrigger::LevelHigh,
        firmware::InterruptTrigger::LevelLow => IrqTrigger::LevelLow,
        firmware::InterruptTrigger::Unknown(_) => IrqTrigger::Unknown(0),
    }
}

/// Walk the PCI hierarchy into domain descriptors (BARs and legacy INTx as
/// named resources, vendor/device as identity).
#[cfg(any(
    feature = "virtio-blk",
    feature = "virtio-net",
    feature = "virtio-rng",
    feature = "virtio-gpu",
    feature = "virtio-input",
    feature = "virtio-vsock",
    feature = "virtio-9p"
))]
pub fn pci_discover() {
    pci::discover::discover(global_domain());
}
