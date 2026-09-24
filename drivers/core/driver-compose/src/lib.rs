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

use alloc::sync::Arc;

use driver_core::{
    Bus, IrqTrigger, ResourceKind, ResourceMap,
    desc::{
        DeviceDescriptor, DeviceIdentity, DeviceLocation, DiscoveryOrigin, FirmwareNodeInfo,
        PlatformIdentity,
    },
    global_domain,
    platform::PlatformBus,
};
use khal::firmware::devices as firmware;
use of;

/// Bring up the new device model.
pub fn init() {
    driver_runtime::XKernelRuntime::install_global();

    // Drop detached class publications when their owning device goes away.
    // One entry point: kclass owns the class list, not this composition
    // root. (New classes become removable by existing in kclass, not by
    // editing this closure.)
    global_domain().set_remove_notifier(Arc::new(|owner| {
        kclass::remove_all_detached(owner);
    }));

    #[cfg(feature = "fixed-clock")]
    drv_clk::fixed_clock::register();
    #[cfg(feature = "serial-pl011")]
    drv_serial::pl011::register();
    #[cfg(feature = "virtio-blk")]
    {
        block::dispatch::reclaimer::init();
        drv_virtio::blk::register();
    }
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
    {
        block::dispatch::reclaimer::init();
        drv_block::ramdisk::register();
    }

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
    enumerate_pci_devices();

    let domain = global_domain();
    let devices = domain.devices();
    let active = devices
        .iter()
        .filter(|snapshot| snapshot.state == driver_core::DeviceState::Active)
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
            source: firmware::FirmwareSource::DeviceTree,
            mmio: node
                .reg()
                .and_then(|mut regs| regs.next())
                .map(|reg| firmware::MmioResource {
                    base: reg.starting_address as usize,
                    size: reg.size,
                }),
            irq: of::first_interrupt_desc(node),
            clock_frequency: node.property_u32("clock-frequency"),
            phandle: node.property_u32("phandle"),
            clock_cells: node.property_u32("#clock-cells"),
            clock_output_name: of::clock_output_name(node),
            clock_specs: of::device_clock_specs(node),
        };
        build_descriptor(dev);
    }
}

fn build_descriptor(dev: firmware::FirmwareDevice) {
    let mut resources = ResourceMap::new();
    if let Some(mmio) = dev.mmio {
        resources.push_kind(ResourceKind::Mmio(driver_core::MmioRegion::new(
            mmio.base, mmio.size,
        )));
    }
    if let Some(irq) = dev.irq {
        resources.push_kind(ResourceKind::Irq(driver_core::IrqSpec::global(
            irq.irq,
            map_trigger(irq.trigger),
        )));
    }
    for spec in &dev.clock_specs {
        let entry_kind = ResourceKind::Clk(driver_core::ClkSpec::by_phandle_index(
            spec.provider as u64,
            spec.index,
        ));
        match spec.connection_name {
            Some(name) => resources.push_named(name, entry_kind),
            None => resources.push_kind(entry_kind),
        }
    }

    let is_provider = dev.clock_cells.is_some();
    let firmware_info = FirmwareNodeInfo {
        phandle: dev.phandle,
        clock_frequency: dev.clock_frequency,
        clock_cells: dev.clock_cells,
        clock_output_name: dev.clock_output_name,
    };

    let descriptor = DeviceDescriptor::new(
        <PlatformBus as Bus>::ID,
        DeviceLocation::FirmwareNode { id: 0 },
        DeviceIdentity::Platform(PlatformIdentity {
            firmware_id: Some(alloc::string::String::from(dev.firmware_id)),
            alias: None,
        }),
    )
    .with_origin(DiscoveryOrigin::DeviceTree)
    .with_resources(resources);
    let descriptor = if is_provider || firmware_info != FirmwareNodeInfo::default() {
        descriptor.with_firmware(firmware_info)
    } else {
        descriptor
    };

    global_domain().add_device(descriptor);
}

/// Board-static devices (no firmware description): the compose layer owns
/// the one static device we migrate; the legacy backend registers the rest.
#[cfg(feature = "serial-ns16550-ioport")]
static COM_PORTS: [u16; 4] = [0x3f8, 0x2f8, 0x3e8, 0x2e8];

#[cfg(feature = "ramdisk")]
fn enumerate_static_devices() {
    use driver_core::desc::{DeviceIdentity, DeviceLocation, PlatformIdentity};
    let descriptor = DeviceDescriptor::new(
        <PlatformBus as Bus>::ID,
        DeviceLocation::PlatformStatic { id: 0 },
        DeviceIdentity::Platform(PlatformIdentity {
            firmware_id: None,
            alias: Some(alloc::string::String::from("ramdisk")),
        }),
    );
    global_domain().add_device(descriptor);
}

#[cfg(not(feature = "ramdisk"))]
fn enumerate_static_devices() {}

fn map_trigger(trigger: firmware::InterruptTrigger) -> IrqTrigger {
    match trigger {
        firmware::InterruptTrigger::EdgeRising => IrqTrigger::EdgeRising,
        firmware::InterruptTrigger::EdgeFalling => IrqTrigger::EdgeFalling,
        firmware::InterruptTrigger::LevelHigh => IrqTrigger::LevelHigh,
        firmware::InterruptTrigger::LevelLow => IrqTrigger::LevelLow,
        firmware::InterruptTrigger::Unknown(_) => IrqTrigger::Unknown,
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
fn enumerate_pci_devices() {
    use driver_core::{
        IrqSpec, MmioRegion, ResourceKind,
        desc::{DeviceIdentity, DeviceLocation, PciIdentity},
    };

    let Ok(mut pci_access) = pci::PciBus::new(pci::support::pci_cam_kind()) else {
        log::warn!("driver-core: PCI access unavailable");
        return;
    };
    let pci_bus_end = pci_access.bus_end();

    let (root, config) = pci_access.parts_mut();
    for bus_nr in 0..=pci_bus_end {
        for (bdf, info) in root.enumerate_bus(bus_nr).collect::<alloc::vec::Vec<_>>() {
            if !matches!(info.header_type, pci::HeaderType::Standard) {
                continue;
            }
            if let Err(err) = pci::support::configure_pci_device_if_needed(root, bdf) {
                log::warn!("driver-core: PCI configure {bdf} failed: {err:?}");
                continue;
            }

            let mut resources = ResourceMap::new();
            let mut bar_idx = 0u8;
            while bar_idx < 6 {
                let Ok(Some(bar)) = root.bar_info(bdf, bar_idx) else {
                    bar_idx += 1;
                    continue;
                };
                let advance = if bar.takes_two_entries() { 2 } else { 1 };
                match bar {
                    pci::BarInfo::Memory { address, size, .. } if size > 0 && address != 0 => {
                        resources.push_kind(ResourceKind::Mmio(MmioRegion::new(
                            address as usize,
                            size as usize,
                        )));
                    }
                    pci::BarInfo::IO { address, size } if size > 0 && address > 0 => {
                        resources.push_kind(ResourceKind::IoPort(driver_core::IoPortRange {
                            base: (address & 0xffff) as u16,
                            size: (size & 0xffff) as u16,
                        }));
                    }
                    _ => {}
                }
                bar_idx += advance;
            }
            if let Some(route) = pci::legacy_interrupt_route(config, bdf) {
                resources.push_kind(ResourceKind::Irq(IrqSpec::global(
                    route.irq,
                    map_trigger(route.trigger),
                )));
            }

            let descriptor = DeviceDescriptor::new(
                <driver_core::pci::PciBus as Bus>::ID,
                DeviceLocation::Pci {
                    segment: 0,
                    bus: bdf.bus,
                    device: bdf.device,
                    function: bdf.function,
                },
                DeviceIdentity::Pci(PciIdentity {
                    vendor_id: info.vendor_id,
                    device_id: info.device_id,
                    class: info.class,
                    subclass: info.subclass,
                }),
            )
            .with_origin(DiscoveryOrigin::PlatformStatic)
            .with_resources(resources);
            global_domain().add_device(descriptor);
        }
    }
}
