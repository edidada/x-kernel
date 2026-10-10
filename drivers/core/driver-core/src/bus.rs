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

use alloc::{format, string::String, vec::Vec};

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
/// Domain-allocated device identity (monotonic, per-`DeviceDomain`).
///
/// A newtype: device ids, class publication ids and devnode ids are three
/// DISTINCT u64 spaces — bare `u64` at every boundary is how the class
/// layer once derived an owner from the wrong space (the off-by-one bug).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub(crate) u64);

impl DeviceId {
    /// Raw value (diagnostics, record keys that tag the space explicitly).
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

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
        // of_device_id semantics: the driver entry matches if it names ANY
        // of the node's compatible strings (a SoC-specific entry followed by
        // the generic IP name is the common DT spelling).
        if platform.compatibles.iter().any(|c| c == self.compatible) {
            MatchScore::Exact
        } else {
            MatchScore::NoMatch
        }
    }
}

#[cfg(unittest)]
mod tests {
    //! of_device_id matching semantics: a driver entry matches if it names
    //! ANY of the node's compatible strings — SoC DT nodes commonly spell
    //! `compatible = "vendor,soc-uart", "arm,pl011"` and the generic IP
    //! driver must still claim the device.

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::bus::{DeviceDescriptor, DeviceIdentity, DeviceLocation};

    fn descriptor_with(compatibles: &[&str]) -> DeviceDescriptor {
        DeviceDescriptor::new(
            crate::platform::PlatformBus::ID,
            DeviceLocation::FirmwareNode { id: 0 },
            DeviceIdentity::Platform(PlatformIdentity {
                firmware_id: compatibles.first().map(|c| String::from(*c)),
                compatibles: compatibles.iter().map(|c| String::from(*c)).collect(),
                alias: None,
            }),
        )
    }

    #[def_test(serial)]
    fn of_match_hits_any_compatible_not_just_primary() {
        let generic_entry = OfMatch::compatible("arm,pl011");
        // SoC-specific primary, generic IP second: must match on the second.
        let soc_node = descriptor_with(&["vendor,soc-uart", "arm,pl011"]);
        assert_eq!(generic_entry.matches(&soc_node), MatchScore::Exact);

        // A node that does not carry the entry's string anywhere: no match.
        let other_node = descriptor_with(&["vendor,other-uart", "vendor,legacy"]);
        assert_eq!(generic_entry.matches(&other_node), MatchScore::NoMatch);
    }
}
