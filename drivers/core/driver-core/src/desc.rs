// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device description vocabulary: what a bus backend hands to the model
//! before any driver binds.
//!
//! One place for bus-stated identity (`DeviceIdentity`), placement
//! (`DeviceLocation`), firmware facts (`FirmwareProp`, `FirmwareNodeInfo`)
//! and the assembled [`DeviceDescriptor`] — the enumeration product.

use alloc::{string::String, vec::Vec};

use crate::{
    bus::{BusId, BusInstanceId},
    device::DeviceId,
    resource::{DmaInfo, ResourceMap},
};

/// Where a device was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryOrigin {
    DeviceTree,
    Acpi,
    PlatformStatic,
}

/// Firmware-stated location of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceLocation {
    Pci {
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
    },
    Mmio {
        base: usize,
        size: usize,
    },
    FirmwareNode {
        id: u32,
    },
    PlatformStatic {
        id: u16,
    },
    Bridge {
        domain: u16,
    },
}

/// Firmware-stated identity used for matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceIdentity {
    /// Firmware node identity: compatible string and/or board alias.
    Platform(PlatformIdentity),
    Pci(PciIdentity),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformIdentity {
    /// Primary (first) `compatible` value, e.g. `"arm,pl011"`. This is what
    /// modalias and diagnostics report.
    pub firmware_id: Option<String>,
    /// The node's full compatible list (`firmware_id` is its first entry).
    /// Matching iterates this list — Linux `of_device_id` semantics: a DT
    /// node `compatible = "vendor,soc-uart", "arm,pl011"` matches a driver
    /// table entry naming EITHER string.
    pub compatibles: Vec<String>,
    /// Board-level alias, e.g. `"serial0"`.
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciIdentity {
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
}

/// One firmware node property, eagerly typed when the encoding is
/// unambiguous and kept as raw bytes otherwise.
///
/// Cell lists longer than two cells arrive as [`FirmwareProp::Bytes``]
/// (a payload of N>2 u32 cells is indistinguishable from an opaque byte
/// string at parse time); [`FirmwareProp::u32_list`] reinterprets them on
/// demand — the parse never guesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirmwareProp {
    /// A zero-length property (`dma-coherent`, `interrupt-controller`):
    /// presence IS the information.
    Empty,
    /// A single NUL-terminated string.
    Str(&'static str),
    /// A NUL-separated string list (`compatible` spellings).
    StrList(Vec<&'static str>),
    /// One big-endian u32 cell.
    U32(u32),
    /// One big-endian u64 value (two cells).
    U64(u64),
    /// Anything else — access via [`FirmwareProp::u32_list`] or the raw
    /// bytes.
    Bytes(&'static [u8]),
}

impl FirmwareProp {
    /// Reinterpret as a list of big-endian u32 cells (DT "cells" arrays:
    /// `dma-ranges` rows, phandle lists). The parse kept such payloads as
    /// [`FirmwareProp::Bytes`] because length alone cannot prove intent.
    pub fn u32_list(&self) -> Option<Vec<u32>> {
        let bytes = match self {
            Self::Bytes(bytes) => *bytes,
            _ => return None,
        };
        if bytes.len() % core::mem::size_of::<u32>() != 0 {
            return None;
        }
        Some(
            bytes
                .chunks_exact(4)
                .map(|cell| u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]))
                .collect(),
        )
    }

    /// The property as a single u32 when it was parsed as one (or is a
    /// one-cell list).
    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Self::U32(value) => Some(*value),
            Self::Bytes(bytes) if bytes.len() == 4 => {
                self.u32_list().and_then(|cells| cells.first().copied())
            }
            _ => None,
        }
    }
}

impl From<u32> for FirmwareProp {
    fn from(value: u32) -> Self {
        Self::U32(value)
    }
}

/// Firmware-node attributes that are not resources: node properties that
/// provider drivers read (`clock-frequency`, `phandle`, `#clock-cells`),
/// plus the full property snapshot every driver can read for quirks
/// ([`FirmwareProp`]).
///
/// Deliberately separate from [`ResourceMap`]: resources are what a consumer
/// acquires; these are what a provider describes itself with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FirmwareNodeInfo {
    /// DT `phandle` (also used as the clock [`ProviderId`](kclk)).
    pub phandle: Option<u32>,
    /// `clock-frequency` property, in Hz (`fixed-clock` nodes).
    pub clock_frequency: Option<u32>,
    /// `#clock-cells`; `Some(_)` marks the node as a clock provider.
    pub clock_cells: Option<u32>,
    /// First `clock-output-names` entry, if present.
    pub clock_output_name: Option<&'static str>,
    /// The node's own properties, firmware order, eagerly typed — the
    /// `of_property_read_*` surface minus a live device-tree. The four
    /// clock fields above are the extracted fast path; everything a
    /// driver's quirk table needs (`vendor,foo-capability`, ...) lives
    /// here, answerable at probe time WITHOUT re-walking firmware.
    pub properties: Vec<(&'static str, FirmwareProp)>,
}

impl FirmwareNodeInfo {
    /// Look up one property snapshot entry by name.
    pub fn prop(&self, name: &str) -> Option<&FirmwareProp> {
        self.properties
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    }
}

#[derive(Debug, Clone)]
pub struct DeviceDescriptor {
    pub bus: BusId,
    /// The CONCRETE bus instance this device sits on (`pci_dev->bus`),
    /// when the bus has instances. Validated at `add_device`: the
    /// instance must exist and its type must equal `bus`.
    pub bus_instance: Option<BusInstanceId>,
    pub parent: Option<u64>,
    pub location: DeviceLocation,
    pub origin: DiscoveryOrigin,
    pub identity: DeviceIdentity,
    pub resources: ResourceMap,
    pub firmware: Option<FirmwareNodeInfo>,
    /// The device's DMA view (mask/offset/coherency), derived from firmware
    /// (`dma-ranges`, `dma-coherent`, PCI bridge windows) at enumeration.
    /// Defaults to [`DmaInfo::DEFAULT`] for board-static devices; consulted
    /// by every DMA allocation — the `dev->dma_mask`/`dev->dma_pfn_offset`
    /// analogue, snapshotted instead of living in the host allocator.
    pub dma: DmaInfo,
}

impl DeviceDescriptor {
    pub fn new(bus: BusId, location: DeviceLocation, identity: DeviceIdentity) -> Self {
        Self {
            bus,
            bus_instance: None,
            parent: None,
            location,
            origin: DiscoveryOrigin::PlatformStatic,
            identity,
            resources: ResourceMap::new(),
            firmware: None,
            dma: DmaInfo::DEFAULT,
        }
    }

    pub fn with_origin(mut self, origin: DiscoveryOrigin) -> Self {
        self.origin = origin;
        self
    }

    pub fn with_resources(mut self, resources: ResourceMap) -> Self {
        self.resources = resources;
        self
    }

    pub fn with_firmware(mut self, firmware: FirmwareNodeInfo) -> Self {
        self.firmware = Some(firmware);
        self
    }

    /// Set the device's DMA view (the `dma-ranges`/`dma-coherent` derived
    /// mask/offset/coherency). Enumerators that found no firmware DMA
    /// facts leave the [`DmaInfo::DEFAULT`] static-device view in place —
    /// but a firmware-DESCRIBED device with no DMA facts is the
    /// enumerator's call (DT nodes get strict facts: non-coherent,
    /// identity, full mask — per what firmware did NOT say).
    pub fn with_dma(mut self, dma: DmaInfo) -> Self {
        self.dma = dma;
        self
    }

    /// Attach to a concrete bus instance (`pci_set_dev` on the bus
    /// object): the instance must have been created with
    /// [`DeviceDomain::add_bus_instance`](crate::DeviceDomain::add_bus_instance)
    /// and its bus type must equal this descriptor's.
    pub fn with_bus_instance(mut self, instance: BusInstanceId) -> Self {
        self.bus_instance = Some(instance);
        self
    }

    /// Set the parent device (the topology link). The domain validates the
    /// parent exists at `add_device` time and cascades removals through
    /// children first — a parent never disappears while a child references
    /// it. `None` (the default) marks root devices (bus hosts, firmware
    /// roots).
    pub fn with_parent(mut self, parent: Option<DeviceId>) -> Self {
        self.parent = parent.map(|id| id.raw());
        self
    }
}
