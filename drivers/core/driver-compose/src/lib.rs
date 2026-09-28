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
use core::sync::atomic::{AtomicU16, Ordering};

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
    pci_discover();

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

/// Monotonic firmware-node location id, allocated per boot in enumeration
/// order (stable within one boot). Every DT node gets a distinct
/// `DeviceLocation::FirmwareNode`, so devnode paths, uevent DEVPATH and
/// class parentage stay unique per device.
static FIRMWARE_NODE_ID: AtomicU16 = AtomicU16::new(0);

fn next_firmware_node_id() -> u16 {
    FIRMWARE_NODE_ID.fetch_add(1, Ordering::Relaxed)
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
            mmios: node
                .reg()
                .map(|regs| {
                    regs.map(|reg| firmware::MmioResource {
                        base: reg.starting_address as usize,
                        size: reg.size,
                    })
                    .collect()
                })
                .unwrap_or_default(),
            irqs: of::interrupt_descs(node),
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
    for mmio in &dev.mmios {
        resources.push_kind(ResourceKind::Mmio(driver_core::MmioRegion::new(
            mmio.base, mmio.size,
        )));
    }
    for irq in &dev.irqs {
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
        DeviceLocation::FirmwareNode {
            id: next_firmware_node_id(),
        },
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
pub fn pci_discover() {
    pci::discover::discover(global_domain());
}
