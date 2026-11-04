// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The platform bus: firmware-node devices and board-static devices.
//!
//! This is the reference bus implementation — the `struct bus_type
//! platform_bus_type` translation. Its vocabulary (identity, firmware
//! property snapshot, static slots, id tables) lives HERE and nowhere
//! else; the model core knows only the [`PlatformBusData`] vtable.
//!
//! Two enumeration sources:
//!
//! - **Firmware nodes** — DT/ACPI-discovered devices (`platform_device`
//!   from `of_node`). Identity is the node's `compatible` list; the
//!   firmware property snapshot rides with the data.
//! - **Board-static devices** — devices the platform describes by fixed
//!   board configuration instead of firmware discovery (`ramdisk`,
//!   ISA COM ports). They are addressed by a board-assigned slot and
//!   claim drivers by board alias, never by compatible.

use alloc::{boxed::Box, format, string::String, vec::Vec};

use super::{Bus, BusDeviceData, BusId};

// ---------------------------------------------------------------------------
// Device data
// ---------------------------------------------------------------------------

/// How a platform device was enumerated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformOrigin {
    /// A firmware (DT/ACPI) node discovered by enumeration. The id is the
    /// enumerator's node handle — monotonic per boot, stable within it.
    FirmwareNode { id: u32 },
    /// A board-configured device: not firmware-discovered, addressed by a
    /// fixed board slot. Claimed by board alias, never by compatible.
    StaticConfig { slot: u16 },
}

/// Platform-bus identity: what a driver's id table matches against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformBusIdentity {
    /// Primary (first) `compatible` value, e.g. `"vendor,generic-ip"`. This is
    /// what modalias and diagnostics report.
    pub firmware_id: Option<String>,
    /// The node's full compatible list (`firmware_id` is its first entry).
    /// Matching iterates this list — Linux `of_device_id` semantics: a DT
    /// node `compatible = "vendor,soc-widget", "vendor,generic-ip"` matches a driver
    /// table entry naming EITHER string.
    pub compatibles: Vec<String>,
    /// Board-level alias, e.g. `"serial0"`. Set for board-static devices
    /// and for firmware nodes carrying an `aliases` entry.
    pub alias: Option<String>,
}

/// The platform bus's device data: enumeration source, identity, and —
/// for firmware nodes — the eagerly-typed property snapshot.
#[derive(Debug, Clone)]
pub struct PlatformBusData {
    /// How (and from where) this device was enumerated.
    pub origin: PlatformOrigin,
    /// Matching identity.
    pub identity: PlatformBusIdentity,
}

impl PlatformBusData {
    /// A DT-discovered node: compatibles in DT order. The node's property
    /// snapshot travels on the descriptor (`DeviceDescriptor::fw`), not in
    /// the bus vocabulary.
    pub fn firmware_node(node_id: u32, compatibles: &[&str]) -> Self {
        Self {
            origin: PlatformOrigin::FirmwareNode { id: node_id },
            identity: PlatformBusIdentity {
                firmware_id: compatibles.first().map(|c| String::from(*c)),
                compatibles: compatibles.iter().map(|c| String::from(*c)).collect(),
                alias: None,
            },
        }
    }

    /// A board-configured device addressed by slot and claimed by alias.
    pub fn static_config(slot: u16, alias: &str) -> Self {
        Self {
            origin: PlatformOrigin::StaticConfig { slot },
            identity: PlatformBusIdentity {
                firmware_id: None,
                compatibles: Vec::new(),
                alias: Some(String::from(alias)),
            },
        }
    }
}

impl BusDeviceData for PlatformBusData {
    fn bus_id(&self) -> BusId {
        PlatformBus::ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn clone_erased(&self) -> Box<dyn BusDeviceData> {
        Box::new(self.clone())
    }

    fn path_component(&self) -> String {
        match self.origin {
            PlatformOrigin::FirmwareNode { id } => format!("platform/firmware{id}"),
            PlatformOrigin::StaticConfig { slot } => format!("platform/static{slot}"),
        }
    }

    fn modalias(&self) -> Option<String> {
        // of_modalias: derived from the DEVICE's first compatible — the
        // same string module loading matches. Board-static devices have
        // no modalias.
        self.identity
            .firmware_id
            .as_ref()
            .map(|compatible| format!("of:NcompatT(null)C{compatible}"))
    }

    fn display_identity(&self) -> String {
        match &self.identity.firmware_id {
            Some(id) => id.clone(),
            None => self.identity.alias.clone().unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Firmware property snapshot
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Id tables
// ---------------------------------------------------------------------------

/// One row of a driver's platform id table: a firmware compatible string.
///
/// Translating a Linux match table is a data edit:
///
/// ```ignore
/// static const struct of_device_id foo_of_match[] = {
///     { .compatible = "vendor,generic-ip" }, {}
/// };
/// ```
///
/// becomes `static IDS: &[OfMatch] = &[OfMatch::compatible("vendor,generic-ip")];`.
#[derive(Debug, Clone, Copy)]
pub struct OfMatch {
    pub compatible: &'static str,
}

impl OfMatch {
    pub const fn compatible(compatible: &'static str) -> Self {
        Self { compatible }
    }
}

/// One board-static alias row (`ramdisk`, `serial0`, ...): matches the
/// device's board alias exactly.
#[derive(Debug, Clone, Copy)]
pub struct AliasMatch {
    pub alias: &'static str,
}

impl AliasMatch {
    pub const fn new(alias: &'static str) -> Self {
        Self { alias }
    }
}

/// The platform bus's id table row (the `platform_device_id` union shape).
#[derive(Debug, Clone, Copy)]
pub enum PlatformId {
    Of(OfMatch),
    Alias(AliasMatch),
}

// ---------------------------------------------------------------------------
// The bus
// ---------------------------------------------------------------------------

/// The platform bus: firmware nodes + board-static devices.
///
/// The reference implementation of [`Bus`]; a dedicated crate eventually.
pub struct PlatformBus;

impl Bus for PlatformBus {
    type DeviceData = PlatformBusData;
    type IdEntry = PlatformId;

    const ID: BusId = BusId("platform");

    fn match_device(data: &Self::DeviceData, entry: &Self::IdEntry) -> bool {
        match entry {
            PlatformId::Of(of) => {
                // of_device_id semantics: the driver entry matches if it
                // names ANY of the node's compatible strings (a
                // SoC-specific entry followed by the generic IP name is
                // the common DT spelling).
                data.identity.compatibles.iter().any(|c| c == of.compatible)
            }
            PlatformId::Alias(alias) => data.identity.alias.as_deref() == Some(alias.alias),
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/platform_bus.rs");
