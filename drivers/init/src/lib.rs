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
    Bus, DmaInfo, IrqTrigger, ResourceKind, ResourceMap,
    desc::DeviceDescriptor,
    global_domain,
    platform::{FirmwareNodeInfo, FirmwareProp, PlatformBus, PlatformBusData},
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
}

fn level1_host_services() {
    driver_host::XKernelRuntime::install_global();
}

fn level2_subsystem_cores() {
    // The block reclaimer is the one subsystem core that needs explicit
    // bring-up before any block driver registers.
    #[cfg(any(feature = "virtio-blk", feature = "ramdisk"))]
    block::dispatch::reclaimer::init();
}

fn level3_drivers() {
    #[cfg(feature = "fixed-clock")]
    drv_clk::fixed_clock::register();
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
        feature = "virtio-9p"
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
        let Some(firmware_id) = node.compatibles().next() else {
            continue;
        };
        let dev = firmware::FirmwareDevice {
            firmware_id,
            all_compatibles: node.compatibles().collect(),
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
            props: of::node_properties(node),
            dma: of::device_dma_facts(node),
        };
        build_descriptor(dev);
    }
}

/// Map a firmware-layer property value into the device model's snapshot
/// vocabulary (firmware crate stays model-agnostic; this is the one
/// translation point).
fn map_prop(value: &firmware::PropValue) -> FirmwareProp {
    match value {
        firmware::PropValue::Empty => FirmwareProp::Empty,
        firmware::PropValue::Str(value) => FirmwareProp::Str(value),
        firmware::PropValue::StrList(list) => FirmwareProp::StrList(list.clone()),
        firmware::PropValue::U32(value) => FirmwareProp::U32(*value),
        firmware::PropValue::U64(value) => FirmwareProp::U64(*value),
        firmware::PropValue::Bytes(bytes) => FirmwareProp::Bytes(bytes),
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

    let firmware_info = FirmwareNodeInfo {
        phandle: dev.phandle,
        clock_frequency: dev.clock_frequency,
        clock_cells: dev.clock_cells,
        clock_output_name: dev.clock_output_name,
        properties: dev
            .props
            .iter()
            .map(|(name, value)| (*name, map_prop(value)))
            .collect(),
    };

    let Some(node_id) = next_firmware_node_id() else {
        return;
    };
    // A firmware-described device ALWAYS carries its node snapshot —
    // the property bag is the quirk surface, not a provider-only extra.
    let mut data = PlatformBusData::firmware_node(node_id, &dev.all_compatibles, firmware_info);
    // The full compatible list: matching iterates it (of_device_id
    // any-match semantics); modalias reports the primary entry.
    data.identity.firmware_id = Some(alloc::string::String::from(dev.firmware_id));
    let descriptor = DeviceDescriptor::new(<PlatformBus as Bus>::ID, alloc::boxed::Box::new(data))
        .with_resources(resources)
        .with_dma(dma_info_from_facts(dev.dma));

    match global_domain().add_device(descriptor) {
        Ok(added) => log::debug!("driver-core: enumerated {added:?}"),
        Err(err) => log::warn!("driver-core: device enumeration rejected: {err:?}"),
    }
}

/// x86 ISA COM port bases (board-static: no firmware description).
#[cfg(feature = "serial-ns16550-ioport")]
static COM_PORTS: [u16; 4] = [0x3f8, 0x2f8, 0x3e8, 0x2e8];

/// Board-static devices (no firmware description): the compose layer is
/// the only registrar since the legacy backend went away — the ramdisk and
/// the x86 COM ports both live here.
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
