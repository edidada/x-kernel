// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PCI bus discovery: enumerate the PCI hierarchy, allocate BARs, resolve
//! legacy IRQs, and register device descriptors into the driver-core domain.
//!
//! This is the PCI bus's entry point into the device model — the composition
//! root calls [`discover`] and nothing else. The bus layer owns the full
//! pipeline from "scan the ECAM window" to "descriptor is in the domain".

use driver_core::{
    Bus, IrqSpec, MmioRegion, ResourceKind, ResourceMap,
    desc::{DeviceDescriptor, DeviceIdentity, DeviceLocation, DiscoveryOrigin, PciIdentity},
};

use crate::{HeaderType, PciBus as PciBusWrapper, support};

/// Discover all PCI devices and register them into the domain.
///
/// Each standard-header function becomes a [`DeviceDescriptor`] with:
/// - `DeviceIdentity::Pci` (vendor/device/class from config space)
/// - MMIO / I/O port resources from assigned BARs
/// - Legacy INTx IRQ (when routed by firmware)
///
/// Bridge devices and functions with unassignable BARs are logged and skipped.
pub fn discover(domain: &driver_core::DeviceDomain) {
    let Ok(mut pci) = PciBusWrapper::new(support::pci_cam_kind()) else {
        log::warn!("pci: bus access unavailable");
        return;
    };

    let bus_end = pci.bus_end();
    let (root, config) = pci.parts_mut();

    for bus_nr in 0..=bus_end {
        for (bdf, info) in root.enumerate_bus(bus_nr) {
            if !matches!(info.header_type, HeaderType::Standard) {
                continue;
            }

            if let Err(err) = support::configure_pci_device_if_needed(root, bdf) {
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
                        resources.push_kind(ResourceKind::IoPort(driver_core::IoPortRange {
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
            if let Some(spec) = resolve_legacy_route(config, bdf) {
                resources.push_kind(ResourceKind::Irq(spec));
            }

            let descriptor = DeviceDescriptor::new(
                <crate::PciPlatformBus as Bus>::ID,
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

            domain.add_device(descriptor);
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

fn map_trigger(trigger: crate::InterruptTrigger) -> driver_core::IrqTrigger {
    use crate::InterruptTrigger as T;
    match trigger {
        T::EdgeRising => driver_core::IrqTrigger::EdgeRising,
        T::EdgeFalling => driver_core::IrqTrigger::EdgeFalling,
        T::LevelHigh => driver_core::IrqTrigger::LevelHigh,
        T::LevelLow => driver_core::IrqTrigger::LevelLow,
        T::Unknown(_) => driver_core::IrqTrigger::Unknown,
    }
}

/// Resolve the legacy INTx irq route for a function into a descriptor-ready
/// [`IrqSpec`]: firmware-described routing (device-tree / ACPI interrupt-map)
/// first, arch static fallback second.
///
/// This centralizes what the transport probe used to re-read per probe:
/// route resolution belongs to the bus (done once at discovery), virtual
/// irq mapping belongs to the runtime provider (done at probe).
fn resolve_legacy_route(
    config: &crate::PciConfigAccess,
    bdf: crate::DeviceFunction,
) -> Option<IrqSpec> {
    if let Some(route) = crate::legacy_interrupt_route(config, bdf) {
        let mut spec = IrqSpec::global(route.irq, map_trigger(route.trigger));
        // khal's firmware enum only knows Gic/Plic today; anything else the
        // firmware stated (or did not) maps to Unknown and the provider
        // falls back to its default resolution.
        spec.controller = Some(match route.controller {
            crate::fw::InterruptControllerKind::Gic => driver_core::IrqControllerKind::Gic,
            crate::fw::InterruptControllerKind::Plic => driver_core::IrqControllerKind::Plic,
            crate::fw::InterruptControllerKind::Unknown => driver_core::IrqControllerKind::Unknown,
        });
        return Some(spec);
    }

    // Static fallback (no firmware routing): legacy PCI INTx ranges used by
    // each QEMU virt-style port. x86 reads the programmed interrupt line
    // from config space; other ports use fixed per-device lines.
    cfg_select! {
        target_arch = "x86_64" => {
            let word = config.read_word(bdf, 0x3C);
            let irq_line = (word & 0xff) as usize;
            if irq_line == 0 || irq_line == 0xff {
                return None;
            }
            let mut spec = IrqSpec::global(irq_line, driver_core::IrqTrigger::LevelLow);
            spec.controller = Some(driver_core::IrqControllerKind::IoApic);
            Some(spec)
        }
        target_arch = "loongarch64" => {
            let mut spec = IrqSpec::global(
                0x10 + (bdf.device & 3) as usize,
                driver_core::IrqTrigger::LevelHigh,
            );
            spec.controller = Some(driver_core::IrqControllerKind::LoongArchExtioi);
            Some(spec)
        }
        target_arch = "aarch64" => {
            let mut spec = IrqSpec::global(
                0x23 + (bdf.device & 3) as usize,
                driver_core::IrqTrigger::LevelHigh,
            );
            spec.controller = Some(driver_core::IrqControllerKind::Gic);
            Some(spec)
        }
        target_arch = "riscv64" => {
            let mut spec = IrqSpec::global(
                0x20 + (bdf.device & 3) as usize,
                driver_core::IrqTrigger::LevelHigh,
            );
            spec.controller = Some(driver_core::IrqControllerKind::Plic);
            Some(spec)
        }
        t => {
            let _ = t;
            None
        }
    }
}
