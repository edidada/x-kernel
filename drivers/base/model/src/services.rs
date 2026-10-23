// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-device service contracts: what a `DeviceAccess` offers a driver.
//!
//! The host implements these over its kernel capabilities; mock services
//! in tests keep the model host-free (L0-b).

use crate::{
    ClkHandle, CoherentMem, DevIrq, IoMap, IrqHandlerFn,
    error::{DepSource, ProbeError, RuntimeError},
    resource::{ClkSpec, IrqSpec, MmioRegion, Virq},
};

/// Borrowed device context supplied by a scoped callback resource view.
///
/// Only the model constructs this context; a long-lived DeviceRef cannot
/// create one. Hosts may inspect enumeration facts and clone the identity for
/// bookkeeping, but retaining identity does not retain binding resources.
/// This is not an operation permit or a liveness lock. The host and driver
/// still own hardware synchronization, quiescence and guard release.
pub struct DeviceContext<'a> {
    device: &'a crate::DeviceRef,
}

impl<'a> DeviceContext<'a> {
    pub(crate) fn new(device: &'a crate::DeviceRef) -> Self {
        Self { device }
    }

    /// Read-only device identity and its authoritative enumeration facts.
    pub fn device(&self) -> &crate::DeviceRef {
        self.device
    }
}

/// Service errors, including deferred dependencies.
///
/// Unlike [`ProbeError`], a service can report that *one* acquisition is
/// deferred while others might still succeed; [`DeviceAccess`](crate::DeviceAccess) folds
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

/// The per-device service surface, implemented by the runtime layer.
///
/// This is a framework-internal seam: drivers touch it only through
/// [`DeviceAccess`](crate::DeviceAccess). It exists so the runtime (and test doubles) can be swapped
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
        device: crate::DeviceContext<'_>,
        region: MmioRegion,
        name: &'static str,
    ) -> Result<IoMap, RuntimeError>;

    /// Register an interrupt for the device (`request_irq` with dev_id
    /// captured in the handler).
    fn request_irq(
        &self,
        device: crate::DeviceContext<'_>,
        spec: IrqSpec,
        name: &'static str,
        handler: IrqHandlerFn,
    ) -> Result<DevIrq, RuntimeError>;

    /// Allocate coherent DMA memory (`dmam_alloc_coherent`) FOR THIS
    /// DEVICE'S DMA VIEW: the context provides the descriptor's
    /// [`DmaInfo`](crate::DmaInfo) so the host applies the device's
    /// `dma-ranges` translation, enforces its `dma_mask`, and runs the
    /// coherency protocol — per-device DMA facts have a home in the
    /// descriptor, not in host-global assumptions.
    /// The resource view rejects unconfigured DMA before dispatch. Hosts must
    /// also return `Unsupported` if the context has no DMA configuration.
    fn alloc_coherent(
        &self,
        device: crate::DeviceContext<'_>,
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
        device: crate::DeviceContext<'_>,
        spec: IrqSpec,
    ) -> Result<Virq, RuntimeError>;
}

/// Clock acquisition — the capability that can DEFER (a provider may not
/// have registered yet), hence its own error vocabulary.
pub trait ClockConsumerServices: Send + Sync {
    /// Acquire a clock input (`clk_get`); may report that the provider has
    /// not registered yet.
    fn clk_get(
        &self,
        device: crate::DeviceContext<'_>,
        spec: ClkSpec,
    ) -> Result<ClkHandle, ServiceError>;
}

/// The host aggregate borrowed by device access and the environment: every mandatory
/// capability, provider publication, plus the host's extension table.
///
/// Capabilities are added as supertraits, never through `as_any`: the host's
/// concrete type is not part of any contract, and family-specific extensions
/// travel by extension TYPE through
/// [`ExtensionServices`](crate::ExtensionServices).
pub trait DeviceServices:
    DeviceResourceServices
    + ClockConsumerServices
    + crate::provider::ProviderServices
    + crate::extension::ExtensionServices
    + Send
    + Sync
{
}

impl From<ServiceError> for ProbeError {
    fn from(err: ServiceError) -> Self {
        match err {
            ServiceError::Deferred(dep) => ProbeError::Deferred(dep),
            ServiceError::Fatal(err) => ProbeError::Fatal(err),
        }
    }
}

impl From<driver_os_api::ResError> for ServiceError {
    fn from(error: driver_os_api::ResError) -> Self {
        Self::Fatal(RuntimeError::from(error))
    }
}
