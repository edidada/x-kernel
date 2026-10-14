// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-device service contracts: what a `DeviceCtx` offers a driver.
//!
//! The host implements these over its kernel capabilities; mock services
//! in tests keep the model host-free (L0-b).

use alloc::sync::Arc;

use crate::{
    error::{DepSource, ProbeError, RuntimeError},
    handle::{ClkHandle, CoherentMem, DevIrq},
    iomap::IoMap,
    resource::{ClkSpec, DmaInfo, IrqSpec, MmioRegion},
};

/// Outcome of an interrupt handler invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqAction {
    /// The interrupt was ours and has been acked.
    Handled,
    /// Not ours (shared line).
    NotHandled,
}

// NOTE: there is deliberately no `WakeThread` variant. kirq has no
// threaded-handler vocabulary yet; a half-supported variant whose
// "wake the thread" half silently degrades to Handled at the runtime
// adapter would let drivers believe they run work in task context while
// actually running it in IRQ context. Re-add it together with a real
// threaded-IRQ path (DeviceServices::request_threaded_irq + a scheduler
// handoff), never as a silent degradation.

/// Service errors, including deferred dependencies.
///
/// Unlike [`ProbeError`], a service can report that *one* acquisition is
/// deferred while others might still succeed; [`DeviceCtx`] folds
/// [`ServiceError::Deferred`] into [`ProbeError::Deferred`] for the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceError {
    Deferred(DepSource),
    Fatal(RuntimeError),
}

impl From<RuntimeError> for ServiceError {
    fn from(err: RuntimeError) -> Self {
        Self::Fatal(err)
    }
}

/// Type-erased interrupt handler registered by [`DeviceCtx::request_irq`].
///
/// The handler carries its own context (the `dev_id` pattern): drivers close
/// over a `Weak` handle into their own `Data`; there is no global irq-to-
/// device routing table anywhere in the framework.
pub type IrqHandlerFn = Arc<dyn Fn() -> IrqAction + Send + Sync>;

/// A family-specific service extension (virtio transport probing, future
/// controller abstractions). The family declares the trait and teaches it
/// how to recover itself from the installed runtime's service surface;
/// driver-core itself never names a device family.
pub trait ExtensionService: Sized {
    /// Recover the extension from the runtime's service object, if the
    /// installed runtime provides this capability.
    fn from_services(services: &dyn DeviceServices) -> Option<Self>;
}

/// The per-device service surface, implemented by the runtime layer.
///
/// This is a framework-internal seam: drivers touch it only through
/// [`DeviceCtx`]. It exists so the runtime (and test doubles) can be swapped
/// without the model depending on any host implementation.
/// MMIO/IRQ/DMA acquisition — the resource capabilities every host must
/// provide. Split from the clock and extension capabilities so the contract
/// states WHAT is mandatory per category instead of one flat "misc
/// interface"; future capabilities (I/O ports, streaming DMA, MSI) join as
/// their own traits, never as more methods on the aggregate.
pub trait DeviceResourceServices: Send + Sync {
    /// Map an MMIO region for the device (`devm_ioremap`).
    fn iomap(
        &self,
        device: u64,
        region: MmioRegion,
        name: &'static str,
    ) -> Result<IoMap, RuntimeError>;

    /// Register an interrupt for the device (`request_irq` with dev_id
    /// captured in the handler).
    fn request_irq(
        &self,
        device: u64,
        spec: IrqSpec,
        name: &'static str,
        handler: IrqHandlerFn,
    ) -> Result<DevIrq, RuntimeError>;

    /// Allocate coherent DMA memory (`dmam_alloc_coherent`) FOR THIS
    /// DEVICE'S DMA VIEW: the caller passes the descriptor's
    /// [`DmaInfo`](crate::DmaInfo) so the host applies the device's
    /// `dma-ranges` translation, enforces its `dma_mask`, and runs the
    /// coherency protocol — per-device DMA facts have a home in the
    /// descriptor, not in host-global assumptions.
    fn alloc_coherent(
        &self,
        device: u64,
        dma: DmaInfo,
        len: usize,
        align: usize,
    ) -> Result<CoherentMem, RuntimeError>;

    /// Resolve an [`IrqSpec`] to its OS-visible virtual irq WITHOUT
    /// registering a handler (`irq_create_of_mapping`): a firmware route
    /// maps through its controller's domain (reusing an existing mapping
    /// when one exists), a resolved virq passes through unchanged.
    ///
    /// Consumers that need the NUMBER itself — a shared-line handler
    /// filter, an MSI-X vector pairing — must resolve here; reading a
    /// firmware route's fields directly confuses the hardware irq with
    /// the logical one on every non-identity domain.
    fn map_irq(&self, device: u64, spec: IrqSpec) -> Result<usize, RuntimeError>;
}

/// Clock acquisition — the capability that can DEFER (a provider may not
/// have registered yet), hence its own error vocabulary.
pub trait ClockConsumerServices: Send + Sync {
    /// Acquire a clock input (`clk_get`); may report that the provider has
    /// not registered yet.
    fn clk_get(&self, device: u64, spec: ClkSpec) -> Result<ClkHandle, ServiceError>;
}

/// The aggregate a [`DeviceCtx`](crate::DeviceCtx) holds: every mandatory
/// capability plus the extension escape hatch.
///
/// `as_any` is THE one deliberate downcast surface: family transports that
/// need host machinery beyond the capability traits (e.g. the virtio PCI
/// transport service) name the runtime type they support and downcast —
/// the core stays family-agnostic. New capabilities become new traits
/// (supertraits here), not new `as_any` consumers.
pub trait DeviceServices: DeviceResourceServices + ClockConsumerServices + Send + Sync {
    /// The runtime's concrete type, for extension-trait downcasts. The core
    /// stays family-agnostic: extensions name the runtime type they support.
    fn as_any(&self) -> &dyn core::any::Any;
}

/// Provider-side clock operations behind a [`ClkHandle`].
///
/// The `kclk` subsystem (the common clock framework core, ported from the `clk/tail` branch
/// acceptance) implements this for real hardware clocks; tests implement it
/// with fixed-rate fakes.
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

impl From<ServiceError> for ProbeError {
    fn from(err: ServiceError) -> Self {
        match err {
            ServiceError::Deferred(dep) => ProbeError::Deferred(dep),
            ServiceError::Fatal(err) => ProbeError::Fatal(err),
        }
    }
}
