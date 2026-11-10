// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The host capability contract (the provider Op traits).
//!
//! Who implements it: the host bridge (`driver-host`'s provider module,
//! binding to memspace/kirq/kdma). Who consumes it: the runtime adapter and
//! the one registered transport exception (E-01, the virtio PCI MSI-X
//! config vector). Living in `driver-base` keeps the dependency direction
//! flat — every consumer already depends on this crate.
//!
//! Note this is a **host-facing** surface, not a driver-facing one, and it is
//! `#[doc(hidden)]` on purpose: a transcribed driver must never see two ways
//! to acquire a resource. Concrete drivers acquire resources through
//! [`crate::Device`] so every guard lands in their `DriverOps::Data`;
//! reaching for these Op traits directly from `drv/**` is the boundary
//! violation described by rule L1 in
//! `docs/ai/driver-subsystem-boundaries.md`, and
//! `scripts/ci/check_driver_boundaries.sh` enforces it.

use alloc::sync::Arc;
use core::ptr::NonNull;

use kirq::IrqHandler;

use crate::error::RuntimeError;
pub use crate::resource::{IrqController, IrqDomainId, IrqTrigger, MmioRegion, Virq};

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
/// Non-shared regular registrations, including current threaded IRQ requests,
/// own the whole IRQ action and are released without a per-action id.
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
    /// Register an interrupt handler for `irq`.
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<IrqHandlerToken>;

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

/// A collection of resources for a single device.
/// Error returned by resource acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResError {
    /// The resource descriptor was invalid (e.g. zero-sized region).
    InvalidResource,
    /// The host failed to establish the requested mapping.
    MappingFailed,
    /// The host ran out of memory while acquiring the resource.
    NoMemory,
    /// The resource is already in use and cannot be shared.
    Busy,
    /// The host does not support this kind of resource.
    Unsupported,
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

pub type ResResult<T = ()> = Result<T, ResError>;

/// The ONE provider-error → driver-error mapping in the tree.
///
/// `ResError` is the host capability vocabulary and `RuntimeError` is what a
/// driver sees; without a single `From` every host bridge re-derives the
/// mapping (and the classifications drift). Hosts call
/// `RuntimeError::from(err)` / `.map_err(RuntimeError::from)`.
impl From<ResError> for crate::error::RuntimeError {
    fn from(err: ResError) -> Self {
        match err {
            ResError::InvalidResource => Self::Invalid,
            ResError::MappingFailed => Self::Io,
            ResError::NoMemory => Self::NoMemory,
            ResError::Busy => Self::Busy,
            ResError::Unsupported => Self::Unsupported,
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

/// A mapping token returned by [`MmioOp::map_mmio`].
///
/// The provider is responsible for interpreting the token in
/// [`MmioOp::unmap_mmio`]. Drivers never construct this directly; they hold an
/// [`Io`] handle instead.
#[derive(Debug, Clone, Copy)]
pub struct MmioMapping {
    /// Virtual address the CPU uses to access the region.
    pub vaddr: usize,
    /// The physical region this mapping covers.
    pub region: MmioRegion,
}

/// MMIO mapping capability: map a physical device region into CPU-visible
/// virtual memory and release it.
///
/// A host kernel implements this and passes it to its driver framework. Runs
/// in normal (non-interrupt) context during device probe and removal.
pub trait MmioOp: Sync {
    /// Map an MMIO region and return a token for later teardown.
    fn map_mmio(&self, region: MmioRegion, name: &'static str) -> ResResult<MmioMapping>;

    /// Release a mapping previously returned by [`Self::map_mmio`].
    fn unmap_mmio(&self, mapping: MmioMapping);
}

/// A request for a coherent DMA buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaSpec {
    /// Buffer length in bytes.
    pub len: usize,
    /// Required alignment in bytes (power of two).
    pub align: usize,
    /// Transfer direction, used by streaming mappings and cache sync.
    pub direction: DmaDirection,
}

impl DmaSpec {
    /// Construct a spec with [`DmaDirection::Bidirectional`] (legacy default).
    pub const fn new(len: usize, align: usize) -> Self {
        Self {
            len,
            align,
            direction: DmaDirection::Bidirectional,
        }
    }

    /// Builder: override the transfer direction.
    pub const fn with_direction(mut self, d: DmaDirection) -> Self {
        self.direction = d;
        self
    }
}

/// A coherent DMA allocation returned by [`DmaOp::alloc_coherent`].
#[derive(Debug, Clone, Copy)]
pub struct DmaAllocation {
    /// Virtual address the CPU uses to access the buffer.
    pub cpu_addr: usize,
    /// Bus address the device uses to access the buffer.
    pub bus_addr: u64,
    /// The originating allocation request.
    pub spec: DmaSpec,
}

/// A streaming DMA mapping returned by [`DmaOp::map_streaming`].
#[derive(Debug, Clone, Copy)]
pub struct DmaMapping {
    /// CPU virtual address of the mapped buffer.
    pub cpu_addr: usize,
    /// Device bus address the device uses to access the buffer.
    pub bus_addr: u64,
    /// Mapped length in bytes.
    pub len: usize,
    /// Transfer direction the mapping was established with.
    pub direction: DmaDirection,
}

pub trait DmaOp: Sync {
    /// Allocate a coherent DMA buffer.
    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation>;

    /// Free a coherent DMA buffer previously returned by [`Self::alloc_coherent`].
    fn free_coherent(&self, alloc: DmaAllocation);

    /// Map an existing caller-owned buffer for streaming DMA.
    ///
    /// Unlike [`Self::alloc_coherent`], the buffer is not owned by the DMA
    /// layer; the mapping is temporary and reversed by [`Self::unmap_streaming`].
    /// Hosts without explicit streaming support may implement this via bounce
    /// buffering. Default: [`Unsupported`](ResError::Unsupported).
    fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<DmaMapping> {
        let _ = (buffer, direction);
        Err(ResError::Unsupported)
    }

    /// Release a streaming mapping previously established by
    /// [`Self::map_streaming`]. Default: no-op.
    fn unmap_streaming(&self, mapping: DmaMapping) {
        let _ = mapping;
    }

    /// Synchronise a streaming mapping before the device reads it (CPU →
    /// device). Default: no-op (coherent / bounce hosts need no maintenance).
    fn sync_for_device(&self, mapping: DmaMapping) {
        let _ = mapping;
    }

    /// Synchronise a streaming mapping before the CPU reads it (device → CPU).
    /// Default: no-op (coherent / bounce hosts need no maintenance).
    fn sync_for_cpu(&self, mapping: DmaMapping) {
        let _ = mapping;
    }

    /// Explicitly flush a CPU address range for non-coherent DMA.
    ///
    /// Default: no-op (bounce-buffer or coherent hosts need no maintenance).
    fn flush_cache(&self, addr: NonNull<u8>, len: usize) {
        let _ = (addr, len);
    }

    /// Explicitly invalidate a CPU address range for non-coherent DMA.
    ///
    /// Default: no-op (bounce-buffer or coherent hosts need no maintenance).
    fn invalidate_cache(&self, addr: NonNull<u8>, len: usize) {
        let _ = (addr, len);
    }
}

/// Direction of a DMA transfer.
///
/// OS-neutral. Host kernels translate this into their own direction
/// representation when performing cache maintenance or bounce buffering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaDirection {
    /// CPU writes, device reads (host flushes before device access).
    DriverToDevice,
    /// Device writes, CPU reads (host invalidates before CPU access).
    DeviceToDriver,
    /// Bidirectional (host flushes and invalidates as needed).
    Bidirectional,
}

/// Provider-side clock operations behind a [`ClkHandle`].
///
/// The `kclk` subsystem (the common clock framework core, ported from the `clk/tail` branch
/// acceptance) implements this for real hardware clocks; tests implement it
/// with fixed-rate fakes.
/// Provider-side phy operations behind a [`crate::PhyHandle`].
///
/// All five are COUNTED consumer-side operations: the phy core (kphy)
/// runs the hardware sequences exactly on the refcount edges, so these
/// map one-to-one onto Linux `phy_init/exit/power_on/power_off/set_mode`.
pub trait PhyOps: Send + Sync {
    fn init(&self) -> Result<(), RuntimeError>;
    fn exit(&self) -> Result<(), RuntimeError>;
    fn power_on(&self) -> Result<(), RuntimeError>;
    fn power_off(&self);
    fn set_mode(&self, mode: u32) -> Result<(), RuntimeError>;
}

/// Provider-side reset-line operations behind a [`crate::ResetHandle`].
///
/// The reset core (kreset) implements this over its registered lines; the
/// host bridges consumer specifiers to it with deferral semantics.
pub trait ResetOps: Send + Sync {
    /// Assert the line (hold the IP in reset).
    fn assert(&self);
    /// Deassert the line (release the IP out of reset).
    fn deassert(&self);
    /// Sample the line: `true` when asserted.
    fn status(&self) -> bool;
}

pub trait ClkOps: Send + Sync {
    fn name(&self) -> &str;

    fn get_rate(&self) -> u64;

    fn round_rate(&self, rate: u64) -> Result<u64, RuntimeError>;

    fn set_rate(&self, rate: u64) -> Result<u64, RuntimeError>;

    fn prepare(&self) -> Result<(), RuntimeError>;

    fn unprepare(&self);

    fn enable(&self) -> Result<(), RuntimeError>;

    fn disable(&self);

    fn is_prepared(&self) -> bool;

    fn is_enabled(&self) -> bool;
}
