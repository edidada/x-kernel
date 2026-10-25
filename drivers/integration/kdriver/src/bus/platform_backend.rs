// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified platform bus backend.
//!
//! Combines firmware-described devices (walked via
//! [`khal::firmware::devices::for_each_compatible`]) and well-known
//! platform-static devices (ramdisk, AHCI, etc.) into a single `platform` bus.
//!
//! All firmware-flavor knowledge (DT vs ACPI) lives behind the
//! `khal::firmware::devices` abstraction, so this backend stays
//! firmware-agnostic.

use driver_base::DriverResult;
use kdevice::{
    BusId, BusTypeId, DeviceIdentity, DeviceLocation, DiscoveryOrigin, IrqResource, MmioRegion,
    PlatformIdentity, ResourceDesc, ResourceSet,
};
use khal::firmware::devices as fw;
use smallvec::smallvec;

use super::{backend::BusBackend, local_id::LocalIdAlloc};
use crate::enumeration::EnumerationContext;

const VIRTIO_MMIO_DT_COMPATIBLE: &str = "virtio,mmio";

/// Unified platform bus backend covering both firmware-described and
/// compile-time-known devices.
pub(crate) struct PlatformBackend {
    id_alloc: LocalIdAlloc,
}

impl PlatformBackend {
    pub(crate) fn new() -> Self {
        Self {
            id_alloc: LocalIdAlloc::new(),
        }
    }
}

impl BusBackend for PlatformBackend {
    fn name(&self) -> &'static str {
        "platform"
    }

    fn bus_type_id(&self) -> BusTypeId {
        BusTypeId::PLATFORM
    }

    fn enumerate(&mut self, context: &mut EnumerationContext, bus_id: BusId) -> DriverResult {
        self.enumerate_firmware(context, bus_id)?;
        self.enumerate_static(context, bus_id)?;
        Ok(())
    }
}

// -- Firmware enumeration (formerly PlatformFirmwareBackend) ------------------

impl PlatformBackend {
    fn enumerate_firmware(
        &mut self,
        context: &mut EnumerationContext,
        bus_id: BusId,
    ) -> DriverResult {
        #[cfg(feature = "virtio-rng")]
        let owner = context.model_owner.clone();
        #[cfg(feature = "virtio-rng")]
        let _discovery = owner.discovery.lock();
        let mut first_error = None;

        let firmware_specs = kdevice::firmware_match_specs_for_bus_type(BusTypeId::PLATFORM);
        #[cfg(feature = "any_firmware_driver")]
        debug_assert!(
            !firmware_specs.is_empty(),
            "platform backend requires firmware-capable platform drivers to be registered before \
             enumeration"
        );

        fw::for_each_compatible_view(
            |compatible| {
                if compatible == VIRTIO_MMIO_DT_COMPATIBLE {
                    Some(VIRTIO_MMIO_DT_COMPATIBLE)
                } else {
                    firmware_specs
                        .iter()
                        .find_map(|spec| spec.match_dt(compatible))
                }
            },
            |view| {
                let dev = view.description();
                let mut resources: ResourceSet = smallvec![];
                if let Some(mmio) = dev.mmio {
                    resources.push(ResourceDesc::Mmio(MmioRegion {
                        base: mmio.base,
                        size: mmio.size,
                    }));
                }
                if let Some(irq) = dev.irq {
                    resources.push(ResourceDesc::Irq(IrqResource::new(
                        irq.irq,
                        super::irq_trigger_from_firmware(irq.trigger),
                    )));
                }

                let registration = if dev.firmware_id == VIRTIO_MMIO_DT_COMPATIBLE {
                    select_virtio_mmio_registration(&view, context)
                } else {
                    let id = match self.id_alloc.alloc() {
                        Ok(id) => id,
                        Err(error) => {
                            first_error.get_or_insert(error);
                            return;
                        }
                    };
                    Some((
                        DeviceLocation::FirmwareNode { id },
                        DeviceIdentity::Platform(PlatformIdentity {
                            alias: None,
                            firmware_id: Some(dev.firmware_id),
                        }),
                        None,
                    ))
                };

                let Some((location, identity, transport)) = registration else {
                    return;
                };

                if let Err(err) = context.register_device(
                    bus_id,
                    location,
                    discovery_origin(dev.source),
                    identity,
                    transport,
                    resources,
                ) {
                    log::warn!(
                        "platform: failed to register {:?} from {:?}: {:?}",
                        identity,
                        dev.source,
                        err
                    );
                    first_error.get_or_insert(err);
                }
            },
        );

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

fn select_virtio_mmio_registration(
    view: &fw::FirmwareDeviceView<'_>,
    context: &mut EnumerationContext,
) -> Option<(
    DeviceLocation,
    DeviceIdentity,
    Option<kdevice::TransportInfo>,
)> {
    if !view.is_available() {
        return None;
    }
    let Some(mmio) = view.description().mmio else {
        log::warn!(
            "platform: skipping {} node without MMIO resource",
            VIRTIO_MMIO_DT_COMPATIBLE
        );
        return None;
    };
    if context.revisit_mmio(mmio.base, mmio.size) {
        return None;
    }
    #[cfg(feature = "virtio")]
    {
        let (kind, version) = virtio_mmio_classification(mmio)?;
        #[cfg(feature = "virtio-rng")]
        if cfg!(target_arch = "aarch64")
            && kind == driver_base::DeviceKind::Hwrng
            && version == virtio::MmioVersion::Modern
            && view.is_root_child_of("linux,dummy-virt")
        {
            context.stage_model_rng(view);
            return None;
        }
        let _ = version;
        virtio_mmio_registration(mmio, kind)
    }
    #[cfg(not(feature = "virtio"))]
    {
        None
    }
}

#[cfg(feature = "virtio")]
fn virtio_mmio_classification(
    mmio: fw::MmioResource,
) -> Option<(driver_base::DeviceKind, virtio::MmioVersion)> {
    let regs = match crate::iomap_mmio(mmio.base, mmio.size, "virtio-mmio-discovery") {
        Ok(regs) => regs,
        Err(err) => {
            log::warn!(
                "platform: failed to map {} at {:#x}: {:?}",
                VIRTIO_MMIO_DT_COMPATIBLE,
                mmio.base,
                err
            );
            return None;
        }
    };

    // SAFETY: iomap_mmio retains a readable mapping of the firmware-described
    // VirtIO region until iounmap below. Classification validates extent and
    // alignment and reads only side-effect-free identification registers.
    let classification = unsafe { virtio::classify_mmio_device(regs.as_ptr(), mmio.size) };
    if let Err(error) = memspace::iounmap(memaddr::VirtAddr::from_usize(regs.as_ptr() as usize)) {
        log::error!(
            "platform: discovery MMIO release failed at {:#x}: {error:?}",
            mmio.base
        );
    }
    classification
}

#[cfg(feature = "virtio")]
fn virtio_mmio_registration(
    mmio: fw::MmioResource,
    device_kind: driver_base::DeviceKind,
) -> Option<(
    DeviceLocation,
    DeviceIdentity,
    Option<kdevice::TransportInfo>,
)> {
    let Some(device_type) =
        crate::driver_registry::virtio::ids::device_kind_to_virtio_type(device_kind)
    else {
        log::debug!(
            "platform: skipping unsupported {} kind {:?} at {:#x}",
            VIRTIO_MMIO_DT_COMPATIBLE,
            device_kind,
            mmio.base,
        );
        return None;
    };

    Some((
        DeviceLocation::Mmio {
            base: mmio.base,
            size: mmio.size,
        },
        DeviceIdentity::Platform(PlatformIdentity {
            alias: None,
            firmware_id: Some(VIRTIO_MMIO_DT_COMPATIBLE),
        }),
        Some(kdevice::TransportInfo::Virtio { device_type }),
    ))
}

fn discovery_origin(source: fw::FirmwareSource) -> DiscoveryOrigin {
    match source {
        fw::FirmwareSource::DeviceTree => DiscoveryOrigin::DeviceTree,
        fw::FirmwareSource::Acpi => DiscoveryOrigin::Acpi,
    }
}

// -- Static enumeration (formerly PlatformStaticBackend) ----------------------

impl PlatformBackend {
    /// `context` and `bus_id` are consumed only inside the per-device `#[cfg]`
    /// blocks below, so a configuration that compiles in no platform-static
    /// device leaves both unused. The names are kept non-underscored because
    /// they are genuine inputs; the allowance below covers configurations
    /// without a matching static device, as in `virtio::activate_virtio_device`.
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: Static enumeration inputs are used only when a matching \
                  platform driver feature is enabled"
    )]
    #[allow(
        unused_variables,
        reason = "CG-024-lint-exceptions: unused_variables: Static enumeration inputs are used \
                  only when a matching platform driver feature is enabled"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "CG-028-focused-functions: TODO(cg): legacy static platform enumeration; split \
                  per device class"
    )]
    fn enumerate_static(
        &mut self,
        context: &mut EnumerationContext,
        bus_id: BusId,
    ) -> DriverResult {
        // --- NS16550 I/O-port (x86 ISA COM, no device tree) ---
        // Always adopt the early stdout instance; the serial driver matches
        // this alias and reuses the port printk already drives.
        #[cfg(feature = "serial-ns16550-ioport")]
        {
            use kdevice::{
                DeviceIdentity, DeviceLocation, DiscoveryOrigin, PlatformIdentity, ResourceSet,
            };
            context.register_device(
                bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc()?,
                },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some("serial-ns16550-ioport"),
                    firmware_id: None,
                }),
                None,
                ResourceSet::new(),
            )?;
        }

        // --- ramdisk ---
        #[cfg(feature = "ramdisk")]
        {
            use kdevice::{
                DeviceIdentity, DeviceLocation, DiscoveryOrigin, PlatformIdentity, ResourceSet,
            };
            context.register_device(
                bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc()?,
                },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some("ramdisk"),
                    firmware_id: None,
                }),
                None,
                ResourceSet::new(),
            )?;
        }

        #[cfg(feature = "any_firmware_driver")]
        let firmware_present = khal::firmware::devices::has_device_description();

        // --- AHCI ---
        #[cfg(feature = "ahci")]
        if !firmware_present {
            use kdevice::{
                DeviceIdentity, DeviceLocation, DiscoveryOrigin, MmioRegion, PlatformIdentity,
                ResourceDesc,
            };
            use smallvec::smallvec;

            use crate::driver_registry::firmware_specs::AHCI;

            context.register_device(
                bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc()?,
                },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some(AHCI.alias),
                    firmware_id: None,
                }),
                None,
                smallvec![ResourceDesc::Mmio(MmioRegion {
                    base: kbuild_config::AHCI_PADDR,
                    size: 0x1000,
                })],
            )?;
        }

        // --- bcm2835-sdhci ---
        #[cfg(feature = "bcm2835-sdhci")]
        if !firmware_present {
            use kdevice::{
                DeviceIdentity, DeviceLocation, DiscoveryOrigin, PlatformIdentity, ResourceSet,
            };

            use crate::driver_registry::firmware_specs::BCM2835_SDHCI;
            context.register_device(
                bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc()?,
                },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some(BCM2835_SDHCI.alias),
                    firmware_id: None,
                }),
                None,
                ResourceSet::new(),
            )?;
        }

        // --- sdmmc ---
        #[cfg(feature = "sdmmc")]
        if !firmware_present {
            use kdevice::{
                DeviceIdentity, DeviceLocation, DiscoveryOrigin, MmioRegion, PlatformIdentity,
                ResourceDesc,
            };
            use smallvec::smallvec;

            use crate::driver_registry::firmware_specs::SDMMC;

            context.register_device(
                bus_id,
                DeviceLocation::PlatformStatic {
                    id: self.id_alloc.alloc()?,
                },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some(SDMMC.alias),
                    firmware_id: None,
                }),
                None,
                smallvec![ResourceDesc::Mmio(MmioRegion {
                    base: kbuild_config::SDMMC_PADDR,
                    size: 0x1000,
                })],
            )?;
        }

        // --- fxmac ---
        // The fxmac driver requires a firmware-provided MMIO window; there
        // is no platform-static base for it in `kbuild_config`. If firmware
        // didn't describe it, skip registration entirely and report loudly
        // rather than registering an empty resource set that would later
        // bottom out at address 0 in the activate path.
        #[cfg(feature = "fxmac")]
        if !firmware_present {
            log::warn!(
                "fxmac: no firmware description and no platform-static MMIO base; skipping \
                 platform fallback registration"
            );
        }

        Ok(())
    }
}
