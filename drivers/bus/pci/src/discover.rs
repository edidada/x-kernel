// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PCI bus discovery: enumerate the PCI hierarchy, allocate BARs, resolve
//! legacy IRQs, and register device descriptors into the driver-core domain.
//!
//! This is the PCI bus's entry point into the device model — the composition
//! root calls [`discover`] and nothing else. The bus layer owns the full
//! pipeline from "scan the ECAM window" to "descriptor is in the domain".

use driver_base::{Bus, IrqSpec, MmioRegion, ResourceKind, ResourceMap, desc::DeviceDescriptor};
use khal::firmware::devices as fw;

use crate::{HeaderType, PciHost, device_setup};

/// Discover all PCI devices and register them into the domain.
///
/// Each standard-header function becomes a [`DeviceDescriptor`] with:
/// - `DeviceIdentity::Pci` (vendor/device/class from config space)
/// - MMIO / I/O port resources from assigned BARs
/// - Legacy INTx IRQ (when routed by firmware)
///
/// Bridge devices and functions with unassignable BARs are logged and skipped.
pub fn discover(domain: &driver_base::DeviceDomain) {
    // A host whose config space is NOT a flat memory window (rk3588 DBI +
    // per-access iATU reprogramming) publishes its access backend while
    // probing; it owns bus numbering relative to the RC (0 = root, 1 = the
    // first downstream bus), so it is scanned as such.
    if let Some(cam) = crate::dw_host::take_published_cam() {
        let segment = fw::pci_host().map_or(0, |host| host.segment);
        log::info!("PCI: discovering through the published DBI+iATU access backend");
        let mut root = crate::PciRoot::new(cam);
        discover_root(domain, &mut root, 0, 1, segment);
        return;
    }

    let Ok(mut pci) = PciHost::new(device_setup::pci_cam_kind()) else {
        log::warn!("pci: bus access unavailable");
        return;
    };

    let bus_start = pci.bus_start();
    let bus_end = pci.bus_end();
    let segment = pci.segment();
    let root = pci.root_mut();
    discover_root(domain, root, bus_start, bus_end, segment);
}

/// The backend-agnostic discovery pipeline: enumerate `bus_start..=bus_end`
/// on `root`, assign BARs, resolve legacy INTx and register descriptors.
///
/// Generic over the configuration access so a flat ECAM window
/// (`MmioCam`) and a reprogram-per-access backend (the rk3588 DBI+iATU
/// `DwCam`) share one implementation instead of forking the pipeline.
pub fn discover_root<C: crate::ConfigurationAccess>(
    domain: &driver_base::DeviceDomain,
    root: &mut crate::PciRoot<C>,
    bus_start: u8,
    bus_end: u8,
    segment: u16,
) {
    // The DMA view every function below this host inherits (see
    // `host_dma_view`): firmware-stated facts, strict reading for a
    // firmware-described host that stated nothing, or the static default
    // with an explicit warning.
    let host_dma = host_dma_view(segment);

    // One ROOT bus instance per discovered host, labeled from the
    // FIRMWARE's own addressing facts (`pci_bus->name` is
    // "<segment>:<bus>" in Linux): every discovered function attaches to
    // it (`pci_dev->bus`). A bridge driver creating a SECONDARY instance
    // registers it anchored on its own bridge device — removing that
    // bridge then ejects the instance's members (the domain's
    // bridge-cascade).
    let root_label = alloc::format!("{segment:04x}:{bus_start:02x}");
    let root_instance = domain
        .add_bus_instance(<crate::PciBus as Bus>::ID, None, &root_label)
        .inspect_err(|err| log::warn!("pci: root bus instance rejected: {err:?}"))
        .ok();

    for bus_nr in bus_start..=bus_end {
        for (bdf, info) in root.enumerate_bus(bus_nr) {
            // Presence filter (PCI spec 6.1: a function that does not exist
            // returns all ones). A backend that instead surfaces its own
            // default ids would otherwise flood the domain with phantom
            // devices -- measured: +256 descriptors before this check.
            if info.vendor_id == 0xffff || info.vendor_id == 0 {
                continue;
            }
            if !matches!(info.header_type, HeaderType::Standard) {
                continue;
            }

            if let Err(err) = device_setup::configure_pci_device_if_needed(root, bdf) {
                log::warn!("pci: configure {bdf} failed: {err:?}");
                continue;
            }

            let mut resources = ResourceMap::new();

            // Collect BAR resources.
            let mut bar_idx = 0u8;
            while bar_idx < 6 {
                let Ok(Some(bar)) = root.bar_info(bdf, bar_idx) else {
                    bar_idx += 1;
                    continue;
                };
                let advance = if bar.takes_two_entries() { 2 } else { 1 };
                match bar {
                    crate::BarInfo::Memory { address, size, .. } if size > 0 && address != 0 => {
                        resources.push_kind(ResourceKind::Mmio(MmioRegion::new(
                            address as usize,
                            size as usize,
                        )));
                    }
                    crate::BarInfo::IO { address, size } if size > 0 && address > 0 => {
                        resources.push_kind(ResourceKind::IoPort(driver_base::IoPortRange {
                            base: (address & 0xffff) as u16,
                            size: (size & 0xffff) as u16,
                        }));
                    }
                    _ => {}
                }
                bar_idx += advance;
            }

            // Legacy INTx route: firmware (OF/ACPI) first, then the arch
            // static fallback. Resolved once here — probe only maps the
            // stored route to a virtual irq.
            if let Some(spec) = resolve_legacy_route(root, bdf) {
                resources.push_kind(ResourceKind::Irq(spec));
            }

            let descriptor = DeviceDescriptor::new(
                <crate::PciBus as Bus>::ID,
                alloc::boxed::Box::new(crate::PciBusData {
                    segment,
                    bus: bdf.bus,
                    device: bdf.device,
                    function: bdf.function,
                    vendor_id: info.vendor_id,
                    device_id: info.device_id,
                    class: info.class,
                    subclass: info.subclass,
                }),
            )
            .with_resources(resources)
            .with_dma(host_dma);
            let descriptor = match root_instance {
                Some(instance) => descriptor.with_bus_instance(instance),
                None => descriptor,
            };

            match domain.add_device(descriptor) {
                Ok(added) => log::debug!("pci: registered {added:?}"),
                Err(err) => log::warn!("pci: device registration rejected: {err:?}"),
            }
            log::debug!(
                "pci: registered {bdf} ({:04x}:{:04x} class {:02x}:{:02x})",
                info.vendor_id,
                info.device_id,
                info.class,
                info.subclass
            );
        }
    }
}

/// The DMA view a discovered host grants every function below it
/// (`of_dma_configure(bridge)` analogue): firmware's `dma-ranges` /
/// `dma-coherent` on the bridge node, with three provenance-honest cases —
/// firmware STATED facts; firmware-described host that stated NOTHING
/// (strict reading: identity, full mask, non-coherent — firmware had its
/// chance); no firmware description at all (static-device default +
/// explicit warning, the same policy as every other static fallback).
fn host_dma_view(segment: u16) -> driver_base::DmaInfo {
    match fw::pci_dma_info() {
        Some(facts) => driver_base::DmaInfo {
            mask: facts.size_mask,
            offset: facts.offset,
            coherent: facts.coherent.unwrap_or(false),
        },
        None if fw::pci_host_source().is_some() => driver_base::DmaInfo {
            mask: u64::MAX,
            offset: 0,
            coherent: false,
        },
        None => {
            log::warn!(
                "pci: no firmware DMA description for host {segment:04x} — defaulting to identity \
                 64-bit coherent (board-static fallback)"
            );
            driver_base::DmaInfo::DEFAULT
        }
    }
}

fn map_trigger(trigger: crate::InterruptTrigger) -> driver_base::IrqTrigger {
    use crate::InterruptTrigger as T;
    match trigger {
        T::EdgeRising => driver_base::IrqTrigger::EdgeRising,
        T::EdgeFalling => driver_base::IrqTrigger::EdgeFalling,
        T::LevelHigh => driver_base::IrqTrigger::LevelHigh,
        T::LevelLow => driver_base::IrqTrigger::LevelLow,
        T::Unknown(_) => driver_base::IrqTrigger::Unknown(0),
    }
}

/// Resolve the legacy INTx irq route for a function into a descriptor-ready
/// [`IrqSpec`]: firmware-described routing (device-tree / ACPI interrupt-map)
/// first, arch static fallback second.
///
/// This centralizes what the transport probe used to re-read per probe:
/// route resolution belongs to the bus (done once at discovery), virtual
/// irq mapping belongs to the runtime provider (done at probe).
fn resolve_legacy_route<C: crate::ConfigurationAccess>(
    root: &crate::PciRoot<C>,
    bdf: crate::DeviceFunction,
) -> Option<IrqSpec> {
    if let Some(route) = crate::legacy_interrupt_route(root, bdf) {
        // A firmware route: the raw line is controller-relative, resolved
        // through the controller's irq domain at request time. khal's
        // firmware enum only knows Gic/Plic today; anything else the
        // firmware stated (or did not) maps to Unknown and the provider
        // falls back to its default resolution.
        let controller = match route.controller {
            crate::fw::InterruptControllerKind::Gic => driver_base::IrqController::Gic,
            crate::fw::InterruptControllerKind::Plic => driver_base::IrqController::Plic,
            crate::fw::InterruptControllerKind::Unknown => driver_base::IrqController::Unknown,
        };
        return Some(IrqSpec::firmware(
            controller,
            route.irq,
            map_trigger(route.trigger),
        ));
    }

    // x86 fallback ONLY: the INTERRUPT_LINE register the firmware
    // (SeaBIOS) programmed into the device — a device-reported fact, not
    // a platform assumption. Other architectures have NO static fallback:
    // INTx routing is firmware's job (DT `interrupt-map` via the path
    // above, ACPI PRT to come); a device with no firmware route simply
    // gets none and must use MSI.
    #[cfg(target_arch = "x86_64")]
    {
        let word = root.configuration_access.read_word(bdf, 0x3C);
        let irq_line = (word & 0xff) as usize;
        if irq_line == 0 || irq_line == 0xff {
            return None;
        }
        Some(IrqSpec::firmware(
            driver_base::IrqController::IoApic,
            irq_line,
            driver_base::IrqTrigger::LevelLow,
        ))
    }
    #[cfg(not(target_arch = "x86_64"))]
    None
}
