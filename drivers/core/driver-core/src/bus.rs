// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The bus abstraction: registration, id tables, matching.

use alloc::{format, string::String};

use crate::desc::{DeviceDescriptor, DeviceIdentity, DeviceLocation};

/// Stable bus type tag (`"platform"`, `"pci"`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BusId(&'static str);

/// Identity of one CONCRETE bus instance (`struct pci_bus`'s number /
/// segment identity, `struct i2c_adapter`'s adapter number): the bridge
/// device that created it plus an addressing label.
///
/// Distinct from the bus TYPE ([`BusId`]) — the type selects the driver
/// family's id table; the instance answers "WHICH bus of that type does
/// this device sit on" (address names, eject groups, instance-scoped
/// rescan). Ids come from the same global space as device ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BusInstanceId(u64);

impl BusInstanceId {
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Raw value (diagnostics, record keys that tag the space explicitly).
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for BusInstanceId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "bus-instance:{}", self.0)
    }
}

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
    use crate::desc::{DeviceDescriptor, DeviceIdentity, DeviceLocation, PlatformIdentity};

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
        // Neutral fixture strings on purpose: rule L0-a says this crate names
        // no device family, and the check is mechanical (no family name on a
        // non-comment line). The module docs above carry the real-world
        // `"vendor,soc-uart", "arm,pl011"` example for readers.
        let generic_entry = OfMatch::compatible("acme-ip,uart");
        // SoC-specific primary, generic IP second: must match on the second.
        let soc_node = descriptor_with(&["acme,soc-uart", "acme-ip,uart"]);
        assert_eq!(generic_entry.matches(&soc_node), MatchScore::Exact);

        // A node that does not carry the entry's string anywhere: no match.
        let other_node = descriptor_with(&["acme,other-uart", "acme,legacy"]);
        assert_eq!(generic_entry.matches(&other_node), MatchScore::NoMatch);
    }
}
