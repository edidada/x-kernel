// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bus abstraction, device descriptors, and the reference platform bus.
//!
//! A bus is *data + matching policy*, not enumeration code: backends live in
//! their own crates and feed [`DeviceDescriptor`]s into a
//! [`DeviceDomain`](crate::DeviceDomain). Matching is driven by const id
//! tables (`&'static [B::IdEntry]`), the mechanical translation target of
//! Linux `pci_device_id` / `of_device_id` arrays.

use alloc::{format, string::String};

use crate::resource::ResourceMap;

/// Stable bus type tag (`"platform"`, `"pci"`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BusId(&'static str);

impl BusId {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    pub const fn name(self) -> &'static str {
        self.0
    }
}

/// How well an id-table entry matches a device.
///
/// Ordered: `Exact > Generic > Fallback > NoMatch`. The domain binds the
/// highest-scoring driver that has not permanently failed the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchScore {
    NoMatch  = 0,
    /// Wildcard/compatible-prefix match.
    Fallback = 1,
    /// Class-level match.
    Generic  = 5,
    /// Specific compatible string or exact vendor/device id.
    Exact    = 10,
}

/// One row of a driver's id table. Implemented by const-constructible data
/// types such as [`OfMatch`].
pub trait IdMatchEntry {
    fn matches(&self, desc: &DeviceDescriptor) -> MatchScore;

    /// The Linux `MODALIAS` string for this row, when the bus derives one
    /// (`pci:v...d...`, `of:N...C<compatible>`). Devices publish it through
    /// their uevent so userland can match drivers by modalias.
    fn modalias(&self) -> Option<String> {
        None
    }
}

/// A bus type: the matching policy and descriptor shape for one bus.
///
/// Implemented by zero-sized marker types in bus crates. Backends that
/// discover devices on the bus use [`Bus::accepts`] to gate descriptors.
pub trait Bus: Sized + Send + Sync + 'static {
    /// Const id-table row type for this bus.
    type IdEntry: IdMatchEntry + Sync;

    /// Stable tag of this bus type.
    const ID: BusId;

    /// Whether a descriptor belongs to this bus at all (location shape).
    fn accepts(desc: &DeviceDescriptor) -> bool;
}

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
        id: u16,
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
    /// `compatible` value, e.g. `"arm,pl011"`.
    pub firmware_id: Option<String>,
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

/// Firmware-node attributes that are not resources: node properties that
/// provider drivers read (`clock-frequency`, `phandle`, `#clock-cells`).
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
}

/// The product of bus enumeration: everything the model knows about a device
/// before any driver binds. Ids are assigned by the
/// [`DeviceDomain`](crate::DeviceDomain) on insertion.
#[derive(Debug, Clone)]
pub struct DeviceDescriptor {
    pub bus: BusId,
    pub parent: Option<u64>,
    pub location: DeviceLocation,
    pub origin: DiscoveryOrigin,
    pub identity: DeviceIdentity,
    pub resources: ResourceMap,
    pub firmware: Option<FirmwareNodeInfo>,
}

impl DeviceDescriptor {
    pub fn new(bus: BusId, location: DeviceLocation, identity: DeviceIdentity) -> Self {
        Self {
            bus,
            parent: None,
            location,
            origin: DiscoveryOrigin::PlatformStatic,
            identity,
            resources: ResourceMap::new(),
            firmware: None,
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
}

/// One platform id-table row: a firmware compatible string or a
/// board-static alias (`struct platform_device_id` union shape).
#[derive(Debug, Clone, Copy)]
pub enum PlatformId {
    Of(OfMatch),
    Alias(AliasMatch),
}

impl IdMatchEntry for PlatformId {
    fn matches(&self, desc: &DeviceDescriptor) -> MatchScore {
        match self {
            Self::Of(of) => of.matches(desc),
            Self::Alias(alias) => alias.matches(desc),
        }
    }

    fn modalias(&self) -> Option<String> {
        match self {
            Self::Of(of) => of.modalias(),
            Self::Alias(_) => None,
        }
    }
}

/// Reference platform bus: firmware nodes, static board devices, and raw MMIO.
pub struct PlatformBus;

impl Bus for PlatformBus {
    type IdEntry = PlatformId;

    const ID: BusId = BusId::new("platform");

    fn accepts(desc: &DeviceDescriptor) -> bool {
        matches!(
            desc.location,
            DeviceLocation::FirmwareNode { .. }
                | DeviceLocation::PlatformStatic { .. }
                | DeviceLocation::Mmio { .. }
        )
    }
}

/// One `struct of_device_id` row: a compatible-string match.
///
/// Translating a Linux match table is a data edit:
///
/// ```ignore
/// static const struct of_device_id foo_of_match[] = {
///     { .compatible = "arm,pl011" }, {}
/// };
/// ```
///
/// becomes `static IDS: &[OfMatch] = &[OfMatch::compatible("arm,pl011")];`.
///
/// `data` (the per-row private pointer in Linux) is not modeled yet; it will
/// arrive as an associated const generic when a ported driver needs it.
#[derive(Debug, Clone, Copy)]
pub struct OfMatch {
    pub compatible: &'static str,
}

impl OfMatch {
    pub const fn compatible(compatible: &'static str) -> Self {
        Self { compatible }
    }
}

/// One platform-static board alias row (`ramdisk`, `serial0`, ...):
/// matches `PlatformIdentity::alias` exactly.
#[derive(Debug, Clone, Copy)]
pub struct AliasMatch {
    pub alias: &'static str,
}

impl AliasMatch {
    pub const fn new(alias: &'static str) -> Self {
        Self { alias }
    }
}

impl IdMatchEntry for AliasMatch {
    fn matches(&self, desc: &DeviceDescriptor) -> MatchScore {
        let DeviceIdentity::Platform(platform) = &desc.identity else {
            return MatchScore::NoMatch;
        };
        match &platform.alias {
            Some(alias) if alias == self.alias => MatchScore::Exact,
            _ => MatchScore::NoMatch,
        }
    }
}

impl IdMatchEntry for OfMatch {
    fn modalias(&self) -> Option<String> {
        Some(format!("of:NcompatT(null)C{}", self.compatible))
    }

    fn matches(&self, desc: &DeviceDescriptor) -> MatchScore {
        let DeviceIdentity::Platform(platform) = &desc.identity else {
            return MatchScore::NoMatch;
        };
        match &platform.firmware_id {
            Some(firmware_id) if firmware_id == self.compatible => MatchScore::Exact,
            _ => MatchScore::NoMatch,
        }
    }
}

/// One `struct pci_device_id` row, for the Phase 3 PCI bus crate.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct PciMatch {
    pub vendor_id: u16,
    pub device_id: u16,
}

#[allow(dead_code)]
impl PciMatch {
    pub const fn device(vendor_id: u16, device_id: u16) -> Self {
        Self {
            vendor_id,
            device_id,
        }
    }

    pub const fn vendor(vendor_id: u16) -> Self {
        Self {
            vendor_id,
            device_id: u16::MAX,
        }
    }
}

impl IdMatchEntry for PciMatch {
    fn modalias(&self) -> Option<String> {
        Some(format!(
            "pci:v{vendor:08X}d{device:08X}",
            vendor = self.vendor_id,
            device = self.device_id
        ))
    }

    fn matches(&self, desc: &DeviceDescriptor) -> MatchScore {
        let DeviceIdentity::Pci(pci) = &desc.identity else {
            return MatchScore::NoMatch;
        };
        let device_match = self.device_id == u16::MAX || self.device_id == pci.device_id;
        if self.vendor_id == pci.vendor_id && device_match {
            if self.device_id == pci.device_id {
                MatchScore::Exact
            } else {
                MatchScore::Generic
            }
        } else {
            MatchScore::NoMatch
        }
    }
}

/// Reference PCI bus: PCI-segment locations with vendor/device identity.
pub struct PciBus;

impl Bus for PciBus {
    type IdEntry = PciMatch;

    const ID: BusId = BusId::new("pci");

    fn accepts(desc: &DeviceDescriptor) -> bool {
        matches!(desc.location, DeviceLocation::Pci { .. })
    }
}
