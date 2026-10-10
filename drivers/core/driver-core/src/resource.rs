// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Firmware-described resources attached to a device.
//!
//! A [`ResourceMap`] is the enumeration product of a bus backend: the facts
//! the platform stated about a device, keyed by index *and* by name. Named
//! lookup mirrors `reg-names` / `interrupt-names` / `clock-names` in device
//! trees, which is the access pattern most Linux drivers use.
//!
//! `DmaSpec`-style allocation *requests* deliberately do not appear here:
//! DMA memory is requested at runtime through
//! [`DeviceCtx::alloc_coherent`](crate::DeviceCtx::alloc_coherent), it is not
//! a firmware fact.

use alloc::{string::String, vec::Vec};

// IRQ vocabulary lives in kirq exactly once (the review's dedup table):
// trigger, controller and domain ids are kirq's domain, not resource.rs's.
pub use kirq::{IrqController, IrqDomainId, IrqTrigger};

/// A physical MMIO region described by firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmioRegion {
    pub base: usize,
    pub size: usize,
}

impl MmioRegion {
    pub const fn new(base: usize, size: usize) -> Self {
        Self { base, size }
    }
}

/// An x86 I/O port range described by firmware or PCI BARs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoPortRange {
    pub base: u16,
    pub size: u16,
}

/// An interrupt as stated by firmware or the bus, before the host's IRQ
/// core resolves it — one of exactly two shapes, never a field-combination
/// convention.
///
/// This mirrors `struct of_phandle_args` / `struct acpi_gsi` for the
/// firmware shape and a resolved gsi/virq for the bus shape. The old struct
/// encoded "which shape am I?" through `number == 0 && hwirq.is_some()`;
/// the enum makes both shapes and their obligations type-level: a
/// [`IrqSpec::ResolvedVirq`] needs no domain translation at request time, a
/// [`IrqSpec::FirmwareRoute`] carries its controller/domain/hwirq triple to
/// the provider unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqSpec {
    /// An OS-wide virtual irq the bus already resolved (an MSI-X vector
    /// returned by transport setup, or a port where the firmware number
    /// already IS the virq).
    ResolvedVirq { number: usize, trigger: IrqTrigger },
    /// A firmware route: controller-relative hwirq (+ optional domain id)
    /// awaiting translation through the controller's irq domain at
    /// request time. Linux keeps DT spec cells until
    /// `irq_create_of_mapping`; compressing early only worked where the
    /// domain map is the identity.
    FirmwareRoute {
        controller: IrqController,
        /// Domain within the controller, when firmware named one. `None`
        /// means single-domain controllers (today's GIC/PLIC roots); the
        /// provider derives the root domain and records it on the mapped
        /// resource.
        domain: Option<IrqDomainId>,
        hwirq: usize,
        trigger: IrqTrigger,
    },
}

impl IrqSpec {
    /// A bus-resolved virtual irq.
    pub const fn resolved(number: usize, trigger: IrqTrigger) -> Self {
        Self::ResolvedVirq { number, trigger }
    }

    /// A firmware route without a stated domain id.
    pub const fn firmware(controller: IrqController, hwirq: usize, trigger: IrqTrigger) -> Self {
        Self::FirmwareRoute {
            controller,
            domain: None,
            hwirq,
            trigger,
        }
    }

    /// The trigger mode of either shape.
    pub const fn trigger(&self) -> IrqTrigger {
        match self {
            Self::ResolvedVirq { trigger, .. } | Self::FirmwareRoute { trigger, .. } => *trigger,
        }
    }

    /// The controller-relative hwirq, if this is a firmware route
    /// (diagnostics: the descriptor's hardware line).
    pub const fn hwirq(&self) -> Option<usize> {
        match self {
            Self::ResolvedVirq { .. } => None,
            Self::FirmwareRoute { hwirq, .. } => Some(*hwirq),
        }
    }
}

/// A clock input described by firmware (`clocks = <&phandle idx>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClkSpec {
    /// Provider identifier (DT phandle of the clock controller).
    pub provider: u64,
    /// Output index within the provider (`clock-specifier` cell).
    pub index: u32,
}

impl ClkSpec {
    pub const fn by_phandle_index(provider: u64, index: u32) -> Self {
        Self { provider, index }
    }
}

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
