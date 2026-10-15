// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-device service contracts: what a `DeviceCtx` offers a driver.
//!
//! The host implements these over its kernel capabilities; mock services
//! in tests keep the model host-free (L0-b).

use crate::{
    devres::{ClkHandle, CoherentMem, DevIrq, IoMap, IrqHandlerFn},
    error::{DepSource, ProbeError, RuntimeError},
    resource::{ClkSpec, DmaInfo, IrqSpec, MmioRegion},
};

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
        device: crate::device::DeviceId,
        region: MmioRegion,
        name: &'static str,
    ) -> Result<IoMap, RuntimeError>;

    /// Register an interrupt for the device (`request_irq` with dev_id
    /// captured in the handler).
    fn request_irq(
        &self,
        device: crate::device::DeviceId,
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
        device: crate::device::DeviceId,
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
    fn map_irq(
        &self,
        device: crate::device::DeviceId,
        spec: IrqSpec,
    ) -> Result<usize, RuntimeError>;
}

/// Clock acquisition — the capability that can DEFER (a provider may not
/// have registered yet), hence its own error vocabulary.
pub trait ClockConsumerServices: Send + Sync {
    /// Acquire a clock input (`clk_get`); may report that the provider has
    /// not registered yet.
    fn clk_get(
        &self,
        device: crate::device::DeviceId,
        spec: ClkSpec,
    ) -> Result<ClkHandle, ServiceError>;
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

impl From<ServiceError> for ProbeError {
    fn from(err: ServiceError) -> Self {
        match err {
            ServiceError::Deferred(dep) => ProbeError::Deferred(dep),
            ServiceError::Fatal(err) => ProbeError::Fatal(err),
        }
    }
}
