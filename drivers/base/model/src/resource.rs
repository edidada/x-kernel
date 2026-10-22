// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Firmware-described resources attached to a device.
//!
//! A [`ResourceMap`] is the enumeration product of a bus backend: the facts
//! the platform stated about a device, keyed by index *and* by name. Named
//! lookup mirrors `reg-names` / `interrupt-names` / `clock-names` in device
//! trees, which is the access pattern most Linux drivers use.
use alloc::{string::String, vec::Vec};

pub use driver_os_api::{
    ClkSpec, DmaInfo, IoPortRange, IrqController, IrqDomainId, IrqSpec, IrqTrigger, MmioRegion,
    Virq,
};

/// The kind of a resource entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Mmio(MmioRegion),
    IoPort(IoPortRange),
    Irq(IrqSpec),
    Clk(ClkSpec),
}

/// One firmware-described resource, optionally named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceEntry {
    pub name: Option<String>,
    pub kind: ResourceKind,
}

impl ResourceEntry {
    pub const fn unnamed(kind: ResourceKind) -> Self {
        Self { name: None, kind }
    }

    pub fn named(name: &str, kind: ResourceKind) -> Self {
        Self {
            name: Some(String::from(name)),
            kind,
        }
    }
}

/// Index- and name-addressable resource set of one device.
///
/// Built by bus backends during enumeration; read-only afterwards. Entries
/// keep declaration order, so positional access mirrors the order of
/// `reg = <...>`, `interrupts = <...>`, `clocks = <...>` properties.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceMap {
    entries: Vec<ResourceEntry>,
}

impl ResourceMap {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn push(&mut self, entry: ResourceEntry) {
        self.entries.push(entry);
    }

    pub fn push_kind(&mut self, kind: ResourceKind) {
        self.entries.push(ResourceEntry::unnamed(kind));
    }

    pub fn push_named(&mut self, name: &str, kind: ResourceKind) {
        self.entries.push(ResourceEntry::named(name, kind));
    }

    pub fn entries(&self) -> &[ResourceEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// n-th MMIO region, in declaration order (`IORESOURCE_MEM, n`).
    pub fn mmio(&self, index: usize) -> Option<MmioRegion> {
        self.entries
            .iter()
            .filter_map(|entry| match entry.kind {
                ResourceKind::Mmio(region) => Some(region),
                _ => None,
            })
            .nth(index)
    }

    /// MMIO region by `reg-names` entry.
    pub fn mmio_by_name(&self, name: &str) -> Option<MmioRegion> {
        self.entries
            .iter()
            .find_map(|entry| match (&entry.name, entry.kind) {
                (Some(entry_name), ResourceKind::Mmio(region)) if entry_name == name => {
                    Some(region)
                }
                _ => None,
            })
    }

    /// n-th interrupt (`platform_get_irq(pdev, n)`).
    pub fn irq(&self, index: usize) -> Option<IrqSpec> {
        self.entries
            .iter()
            .filter_map(|entry| match entry.kind {
                ResourceKind::Irq(spec) => Some(spec),
                _ => None,
            })
            .nth(index)
    }

    /// Interrupt by `interrupt-names` entry.
    pub fn irq_by_name(&self, name: &str) -> Option<IrqSpec> {
        self.entries
            .iter()
            .find_map(|entry| match (&entry.name, entry.kind) {
                (Some(entry_name), ResourceKind::Irq(spec)) if entry_name == name => Some(spec),
                _ => None,
            })
    }

    /// Clock input by `clock-names` entry (`clk_get(dev, id)`).
    pub fn clk_by_name(&self, name: &str) -> Option<ClkSpec> {
        self.entries
            .iter()
            .find_map(|entry| match (&entry.name, entry.kind) {
                (Some(entry_name), ResourceKind::Clk(spec)) if entry_name == name => Some(spec),
                _ => None,
            })
    }

    /// n-th clock input, in `clocks = <...>` declaration order.
    pub fn clk_at(&self, index: usize) -> Option<ClkSpec> {
        self.entries
            .iter()
            .filter_map(|entry| match entry.kind {
                ResourceKind::Clk(spec) => Some(spec),
                _ => None,
            })
            .nth(index)
    }
}

impl FromIterator<ResourceEntry> for ResourceMap {
    fn from_iter<I: IntoIterator<Item = ResourceEntry>>(iter: I) -> Self {
        Self {
            entries: iter.into_iter().collect(),
        }
    }
}
