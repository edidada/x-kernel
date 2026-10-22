// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The IRQ family: handler callback vocabulary and the request guard.

use alloc::sync::Arc;
use core::fmt;

pub use irq_types::{IrqController, IrqDomainId, IrqTrigger};

use super::ReleaseHook;
use crate::{ResError, ResResult};

/// A registered device interrupt; released (handler freed, line freed) on
/// drop.
pub struct DevIrq {
    virq: Virq,
    release: Option<ReleaseHook>,
}

impl fmt::Debug for DevIrq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DevIrq")
            .field("virq", &self.virq)
            .finish_non_exhaustive()
    }
}

impl DevIrq {
    /// Construct a guard for a host registration with its matching release hook.
    pub fn new(virq: Virq, release: Option<ReleaseHook>) -> Self {
        Self { virq, release }
    }

    pub const fn virq(&self) -> Virq {
        self.virq
    }
}

impl Drop for DevIrq {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Outcome of an interrupt handler invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqAction {
    /// The interrupt was ours and has been acked.
    Handled,
    /// Not ours (shared line).
    NotHandled,
}

/// Type-erased ordinary device interrupt handler.
///
/// Runs in normal device IRQ context; it must not sleep. This is not a
/// pseudo-NMI or threaded-IRQ callback. Host adapters translate its result
/// to their IRQ core's vocabulary.
///
/// The handler carries its own context (the `dev_id` pattern): drivers close
/// over a `Weak` handle into their own `Data`; there is no global irq-to-
/// device routing table anywhere in the framework.
pub type IrqHandlerFn = Arc<dyn Fn() -> IrqAction + Send + Sync>;

/// An OS-visible virtual interrupt number (the number the host IRQ core
/// dispatches).
///
/// Firmware reports controller-relative **hwirq** numbers, the bus may
/// already carry a **virq**, and consumers (a shared-line filter, an MSI-X
/// vector pairing, a uevent) need one or the other. Both used to be `usize`,
/// which is exactly the confusion that made IRQ routing bugs invisible on
/// identity-mapped QEMU. The newtype keeps them apart: only [`Virq::raw`]
/// crosses into a plain number, and only at a boundary that demands one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Virq(usize);

impl Virq {
    /// Wrap a number the host IRQ core already resolved.
    pub const fn new(number: usize) -> Self {
        Self(number)
    }

    /// The raw number, for APIs that predate this type (kernel IRQ core,
    /// vendored device libraries) — the single explicit crossing point.
    pub const fn raw(self) -> usize {
        self.0
    }
}

impl core::fmt::Display for Virq {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
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
    ResolvedVirq { virq: Virq, trigger: IrqTrigger },
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
    pub const fn resolved(virq: Virq, trigger: IrqTrigger) -> Self {
        Self::ResolvedVirq { virq, trigger }
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

    /// The OS-visible virtual irq, if this spec already carries one.
    pub const fn virq(&self) -> Option<Virq> {
        match self {
            Self::ResolvedVirq { virq, .. } => Some(*virq),
            Self::FirmwareRoute { .. } => None,
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

/// Device-visible MSI message returned by the host IRQ core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]

pub struct MsiMessage {
    /// Message address programmed into the device.
    pub address: u64,
    /// Message data programmed into the device.
    pub data: u32,
}

impl MsiMessage {
    /// Creates a device-visible MSI message.
    pub const fn new(address: u64, data: u32) -> Self {
        Self { address, data }
    }
}

/// An interrupt resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqResource {
    /// A plain virtual irq with no retained origin.
    PlainVirq { virq: Virq, trigger: IrqTrigger },
    /// A domain-mapped irq that remembers its firmware origin: the
    /// controller/domain/hwirq the mapping went through — identity audits
    /// and release go through the same domain the mapping came from,
    /// never a re-derived guess. The mapping has HAPPENED: `virq` is
    /// the real virq (request paths resolve firmware routes through
    /// `map_irq` before registering — no pre-mapping placeholder state
    /// exists in this shape anymore).
    DomainMapped {
        /// OS-visible irq number (the domain translation's output).
        virq: Virq,
        trigger: IrqTrigger,
        controller: IrqController,
        /// The domain the mapping used: firmware-stated, or the
        /// controller's root domain when firmware named none (single-domain
        /// controllers).
        domain: Option<IrqDomainId>,
        hwirq: usize,
    },
}

/// MSI/MSI-X interrupt resource allocated by the host IRQ core.
#[derive(Debug, PartialEq, Eq)]
pub struct MsiResource {
    /// OS-visible IRQ used for handler registration.
    pub irq: IrqResource,
    /// Device-visible message to program into MSI/MSI-X registers.
    pub message: MsiMessage,
}

/// A discovered interrupt route awaiting translation into an [`IrqResource`].
///
/// Firmware (device-tree / ACPI) or bus discovery produces this raw
/// description; the provider's [`IrqOp::map_irq`] turns it into the OS-visible
/// [`IrqResource`].
#[derive(Debug, Clone, Copy)]
pub struct IrqRouteDesc {
    /// Hardware IRQ number as discovered (e.g. GIC SPI number, PLIC source).
    pub hwirq: usize,
    /// Trigger mode.
    pub trigger: IrqTrigger,
    /// Controller family that owns this route.
    pub controller: IrqController,
    /// Domain id, when the host partitions IRQ number space.
    pub domain: Option<IrqDomainId>,
}

/// Provider-owned identity for one registered interrupt handler.
///
/// Shared hardirq registrations are released by the provider-local action id.
/// Non-shared regular registrations own the whole IRQ action and are released
/// without a per-action id. This model has no threaded registration entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqHandlerToken {
    /// One handler within a shared IRQ action list.
    SharedAction(usize),
    /// The sole regular action installed on an IRQ line.
    RegularAction,
}

/// Interrupt capability: handler registration, interrupt-domain translation,
/// and MSI-X vector allocation.
///
/// A host kernel implements this and passes it to its driver framework. The
/// domain-translation and MSI-X methods have default implementations so a host
/// only overrides what its interrupt controller supports (e.g. non-x86 leaves
/// MSI-X as `Unsupported`).
pub trait IrqOp: Send + Sync {
    /// Register a normal device interrupt handler for the resolved `irq`.
    ///
    /// The handler captures its device context and returns IrqAction. The host
    /// adapts this to its own callback and result types; it must not dispatch
    /// this callback through a pseudo-NMI path. Invocation must not sleep.
    fn request_irq(&self, irq: IrqResource, handler: IrqHandlerFn) -> ResResult<IrqHandlerToken>;

    /// Release an interrupt handler previously registered for `irq`.
    fn release_irq(&self, irq: IrqResource, token: IrqHandlerToken);

    /// Enable or disable delivery of `irq`.
    fn set_irq_enabled(&self, irq: IrqResource, enabled: bool);

    /// Translate a discovered interrupt route into an OS-visible IRQ resource.
    ///
    /// Default: treat the hardware number as the OS-visible number with no
    /// translation.
    fn map_irq(&self, route: IrqRouteDesc) -> ResResult<IrqResource> {
        Ok(IrqResource::mapped(
            Virq::new(route.hwirq),
            route.trigger,
            route.controller,
            route.domain,
            route.hwirq,
        ))
    }

    /// Allocate a PCI MSI-X interrupt resource.
    ///
    /// The returned IRQ is OS-visible and suitable for handler registration.
    /// The returned message is device-visible and suitable for programming an
    /// MSI-X table entry. Controller-local vectors and APIC destination details
    /// remain host IRQ-core implementation details.
    fn alloc_msix(&self) -> ResResult<MsiResource> {
        Err(ResError::Unsupported)
    }

    /// Release an MSI-X resource previously allocated by [`Self::alloc_msix`].
    /// Default: no-op.
    fn free_msix(&self, resource: MsiResource) {
        let _ = resource;
    }
}

impl IrqResource {
    /// A plain (origin-less) virtual irq.
    pub const fn plain(virq: Virq, trigger: IrqTrigger) -> Self {
        Self::PlainVirq { virq, trigger }
    }

    /// A domain-mapped irq with its firmware origin.
    pub const fn mapped(
        virq: Virq,
        trigger: IrqTrigger,
        controller: IrqController,
        domain: Option<IrqDomainId>,
        hwirq: usize,
    ) -> Self {
        Self::DomainMapped {
            virq,
            trigger,
            controller,
            domain,
            hwirq,
        }
    }

    /// The OS-visible virtual irq of either shape.
    pub const fn virq(&self) -> Virq {
        match self {
            Self::PlainVirq { virq, .. } | Self::DomainMapped { virq, .. } => *virq,
        }
    }

    /// The trigger mode of either shape.
    pub const fn trigger(&self) -> IrqTrigger {
        match self {
            Self::PlainVirq { trigger, .. } | Self::DomainMapped { trigger, .. } => *trigger,
        }
    }
}

impl MsiResource {
    /// Creates an MSI resource from an IRQ and device message.
    pub const fn new(irq: IrqResource, message: MsiMessage) -> Self {
        Self { irq, message }
    }
}

impl IrqHandlerToken {
    /// Create a token for one shared IRQ action.
    pub const fn shared_action(id: usize) -> Self {
        Self::SharedAction(id)
    }

    /// Create a token for a non-shared regular IRQ action.
    pub const fn regular_action() -> Self {
        Self::RegularAction
    }
}

unittest::path_mod_test!(tests, "../ut-tests/irq.rs");
