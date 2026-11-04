// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The bus abstraction: the seam where bus-specific vocabulary enters the
//! model.
//!
//! The core knows exactly one thing about a bus — that it owns its
//! device-data vocabulary. [`BusDeviceData`] is the erased vtable the
//! domain's single heterogeneous device table dispatches through; the
//! typed [`Bus`] side (associated types + [`Bus::match_device`]) is used
//! wherever the bus is statically known (`DeviceAccess`, `DriverOps`).

pub mod platform_bus;

use alloc::{boxed::Box, string::String};

pub use platform_bus::{PlatformBus, PlatformBusData, PlatformId};

/// Stable identity of a bus type (the `bus_type.name` analogue).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusId(pub &'static str);

impl BusId {
    pub const fn name(self) -> &'static str {
        self.0
    }
}

/// Identity of one concrete bus instance (`pci_set_dev` addressing
/// anchor). See [`DeviceDomain::add_bus_instance`](crate::DeviceDomain::add_bus_instance).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BusInstanceId(u64);

impl BusInstanceId {
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) const fn raw(self) -> u64 {
        self.0
    }
}

/// The per-bus device-data vtable: what the core can ask of ANY device's
/// data without knowing its bus.
///
/// This is the Rust translation of Linux's `bus_type` function pointers
/// (`dev_name`, `dev_uevent`, `match`): a small, closed set of generic
/// operations. The payload type itself is bus vocabulary and lives with
/// its bus (e.g. [`PlatformBusData`]) — the core never inspects it.
/// Registered data is retained by device references after removal. It must
/// contain only descriptive facts, not binding-owned hardware resources, and
/// its destructor must not require a sleepable context.
pub trait BusDeviceData: core::fmt::Debug + Send + Sync + 'static {
    /// The bus that owns this vocabulary.
    fn bus_id(&self) -> BusId;

    /// The type-erased self, for the ONE guarded downcast the domain
    /// performs (recovering a bus's concrete data after a `bus_id`
    /// check). Mirrors how the extension surface recovers host machinery.
    fn as_any(&self) -> &dyn core::any::Any;

    /// Clone the payload as its own boxed trait object.
    fn clone_erased(&self) -> Box<dyn BusDeviceData>;

    /// Bus-defined location component, stable for this device's lifetime.
    /// The framework neither composes parent paths nor enforces uniqueness;
    /// devnode naming and namespace collision checks belong to their publisher.
    fn path_component(&self) -> String;

    /// The device-derived MODALIAS (uevent/module loading). Platform-bus
    /// devices derive it from their compatible string (of_modalias
    /// semantics); buses without a modalias rule return `None`.
    fn modalias(&self) -> Option<String>;

    /// One-line identity for diagnostics (`dev_name` + firmware id).
    fn display_identity(&self) -> String;
}

/// One bus type. Owns its device-data vocabulary and its matching rule.
///
/// `DeviceData` and `IdEntry` are the vocabulary this bus contributes:
/// enumeration products carry `DeviceData`, driver tables carry `IdEntry`
/// rows, and [`Bus::match_device`] is the bus's own matching rule
/// (`bus_type.match`) — the core never sees either shape.
pub trait Bus: Sized + Send + Sync + 'static {
    /// Enumeration product for devices on this bus.
    type DeviceData: BusDeviceData;

    /// One row of a driver's const id table on this bus.
    type IdEntry: Send + Sync + 'static;

    /// Stable identity of this bus type.
    const ID: BusId;

    /// Whether enumeration facts satisfy one driver id-table row.
    ///
    /// A match permits probing; it does not rank drivers or promise a binding.
    /// Matching must not acquire hardware resources. Runtime dependencies are
    /// resolved by probe, which can defer until they become available.
    fn match_device(data: &Self::DeviceData, entry: &Self::IdEntry) -> bool;
}

/// Format the bus-defined location component of a device.
///
/// This does not compose parent topology or a sysfs namespace. It is neither
/// a complete Linux DEVPATH nor a globally unique device/publication key.
pub fn physical_path(data: &dyn BusDeviceData) -> alloc::string::String {
    data.path_component()
}
