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

use alloc::vec::Vec;

use device_res::{ClkProviderDesc, ClkResource, ClkSpec};
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
pub struct PlatformBackend {
    id_alloc: LocalIdAlloc,
}

impl PlatformBackend {
    pub fn new() -> Self {
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
        let mut first_error = None;

        let firmware_specs = kdevice::firmware_match_specs_for_bus_type(BusTypeId::PLATFORM);
        #[cfg(feature = "any_firmware_driver")]
        debug_assert!(
            !firmware_specs.is_empty(),
            "platform backend requires firmware-capable platform drivers to be registered before \
             enumeration"
        );

        // Clock providers (nodes with #clock-cells) must be registered — and
        // therefore probed — before their consumers: a consumer's probe
        // resolves `clocks` references through the provider registry (Linux
        // solves the same ordering with of_clk_init + EPROBE_DEFER; here
        // provider-first registration makes one probe pass sufficient).
        let mut clock_providers: Vec<fw::FirmwareDevice> = Vec::new();
        let mut other_devices: Vec<fw::FirmwareDevice> = Vec::new();

        fw::for_each_compatible(
            |compatible| {
                if compatible == VIRTIO_MMIO_DT_COMPATIBLE {
                    Some(VIRTIO_MMIO_DT_COMPATIBLE)
                } else {
                    firmware_specs
                        .iter()
                        .find_map(|spec| spec.match_dt(compatible))
                }
            },
            |dev| {
                if dev.clock_cells.is_some() {
                    clock_providers.push(dev);
                } else {
                    other_devices.push(dev);
                }
            },
        );

        for dev in clock_providers.into_iter().chain(other_devices) {
            {
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
                if let (Some(provider_id), Some(clock_cells)) = (dev.phandle, dev.clock_cells) {
                    let mut provider = ClkProviderDesc::new(provider_id, clock_cells);
                    if let Some(rate_hz) = dev.clock_frequency {
                        provider = provider.with_fixed_rate(u64::from(rate_hz));
                    }
                    if let Some(name) = dev.clock_output_name {
                        provider = provider.with_output_name(name);
                    }
                    resources.push(ResourceDesc::ClkProvider(provider));
                }

                let registration = if dev.firmware_id == VIRTIO_MMIO_DT_COMPATIBLE {
                    match dev.mmio {
                        #[cfg(feature = "virtio")]
                        Some(mmio) => virtio_mmio_registration(mmio),
                        #[cfg(not(feature = "virtio"))]
                        Some(_) => None,
                        None => {
                            log::warn!(
                                "platform: skipping {} node without MMIO resource",
                                VIRTIO_MMIO_DT_COMPATIBLE
                            );
                            None
                        }
                    }
                } else {
                    Some((
                        DeviceLocation::FirmwareNode {
                            id: self.id_alloc.alloc(),
                        },
                        DeviceIdentity::Platform(PlatformIdentity {
                            alias: None,
                            firmware_id: Some(dev.firmware_id),
                        }),
                        None,
                    ))
                };

                let Some((location, identity, transport)) = registration else {
                    continue;
                };

                // Consumer `clocks` references become Clk resources, so
                // drivers resolve them through first_clk()/clk_at() exactly
                // like MMIO and IRQ.
                for spec in &dev.clock_specs {
                    let clk_spec = ClkSpec::by_phandle_index(spec.provider, spec.index);
                    let resource = match spec.connection_name {
                        Some(name) => ClkResource::named(name, clk_spec),
                        None => ClkResource::unnamed(clk_spec),
                    };
                    resources.push(ResourceDesc::Clk(resource));
                }

                if let Err(err) = context.register_device(
                    bus_id,
                    location,
                    discovery_origin(dev.source),
                    identity,
                    transport,
                    resources,
                    dev.node.and_then(OfFirmwareNode::shared),
                ) {
                    log::warn!(
                        "platform: failed to register {:?} from {:?}: {:?}",
                        identity,
                        dev.source,
                        err
                    );
                    first_error.get_or_insert(err);
                }
            }
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

#[cfg(feature = "virtio")]
fn virtio_mmio_registration(
    mmio: fw::MmioResource,
) -> Option<(
    DeviceLocation,
    DeviceIdentity,
    Option<kdevice::TransportInfo>,
)> {
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

    // SAFETY: `regs` was obtained from `iomap_mmio` which maps a valid
    // physical MMIO region, and `mmio.size` matches the region size.
    let Some((device_kind, _transport)) =
        (unsafe { virtio::probe_mmio_device(regs.as_ptr(), mmio.size) })
    else {
        log::trace!(
            "platform: skipping empty {} slot at {:#x}",
            VIRTIO_MMIO_DT_COMPATIBLE,
            mmio.base,
        );
        return None;
    };

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
    /// they are genuine inputs; `#[allow(unused_variables)]` covers the empty
    /// configuration (same convention as `virtio::activate_virtio_device`).
    #[allow(unused_variables)]
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
                    id: self.id_alloc.alloc(),
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
                    id: self.id_alloc.alloc(),
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
                    id: self.id_alloc.alloc(),
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
                    id: self.id_alloc.alloc(),
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
                    id: self.id_alloc.alloc(),
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

// ---------------------------------------------------------------------------
// FirmwareNode bridge (DT on-demand property reads for probes)
// ---------------------------------------------------------------------------

/// [`kdevice::FirmwareNode`] adapter over one DT node — the Linux
/// `dev.of_node` pattern: probes read provider registration parameters
/// from their own node on demand instead of carrying pre-extracted copies
/// in the resource list. Lives here (not in the `of` crate) because the
/// device tree crate must not depend on the device-model contract; this
/// integration layer already depends on both.
struct OfFirmwareNode(of::FdtNode<'static, 'static>);

impl OfFirmwareNode {
    fn shared(
        node: of::FdtNode<'static, 'static>,
    ) -> Option<alloc::sync::Arc<dyn kdevice::FirmwareNode>> {
        Some(alloc::sync::Arc::new(Self(node)) as alloc::sync::Arc<dyn kdevice::FirmwareNode>)
    }
}

impl kdevice::FirmwareNode for OfFirmwareNode {
    fn read_u32(&self, name: &str) -> Option<u32> {
        of::property_u32(self.0, name)
    }

    fn read_str(&self, name: &str) -> Option<&'static str> {
        // Firmware string-list properties are NUL-separated; the first
        // entry is the meaningful one for single-output lookups.
        let prop = self.0.property(name)?;
        let end = prop
            .value
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(prop.value.len());
        core::str::from_utf8(&prop.value[..end]).ok()
    }
}
