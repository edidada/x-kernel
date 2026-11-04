// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ resource types: trigger/controller/domain/resource description, the
//! [`IrqEvent`] outcome, the [`IrqHandler`] / [`IrqOp`] capability traits, and
//! the [`Irq`] RAII handle.

use alloc::sync::Arc;

pub use driver_os_api::{
    IRQ_EVENT_SOURCES, IrqController, IrqDomainId, IrqEvent, IrqEventSource, IrqHandler,
    IrqHandlerToken, IrqRouteDesc, IrqThreadHandler, IrqTrigger, MsiMessage,
};

use crate::{ResError, ResResult};

/// An interrupt resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqResource {
    /// IRQ number visible to the OS (virtual IRQ after domain translation).
    pub number: usize,
    /// Trigger mode.
    pub trigger: IrqTrigger,
    /// Controller family that owns this IRQ, if known.
    pub controller: Option<IrqController>,
    /// Domain this IRQ belongs to, if the host partitions IRQ number space.
    pub domain: Option<IrqDomainId>,
    /// Hardware IRQ number within `controller` / `domain`, when distinct from
    /// the OS-visible `number`.
    pub hwirq: Option<usize>,
}

impl IrqResource {
    /// Construct a minimal IRQ resource with just a number and trigger.
    pub const fn new(number: usize, trigger: IrqTrigger) -> Self {
        Self {
            number,
            trigger,
            controller: None,
            domain: None,
            hwirq: None,
        }
    }

    /// Builder: attach a controller family.
    pub const fn with_controller(mut self, c: IrqController) -> Self {
        self.controller = Some(c);
        self
    }

    /// Builder: attach a domain id.
    pub const fn with_domain(mut self, d: IrqDomainId) -> Self {
        self.domain = Some(d);
        self
    }

    /// Builder: attach the hardware IRQ number.
    pub const fn with_hwirq(mut self, n: usize) -> Self {
        self.hwirq = Some(n);
        self
    }
}

/// MSI/MSI-X interrupt resource allocated by the host IRQ core.
#[derive(Debug, PartialEq, Eq)]
pub struct MsiResource {
    /// OS-visible IRQ used for handler registration.
    pub irq: IrqResource,
    /// Device-visible message to program into MSI/MSI-X registers.
    pub message: MsiMessage,
}

impl MsiResource {
    /// Creates an MSI resource from an IRQ and device message.
    pub const fn new(irq: IrqResource, message: MsiMessage) -> Self {
        Self { irq, message }
    }
}

/// Interrupt capability: handler registration, interrupt-domain translation,
/// and MSI-X vector allocation.
///
/// A host kernel implements this and passes it to its driver framework. The
/// domain-translation and MSI-X methods have default implementations so a host
/// only overrides what its interrupt controller supports (e.g. non-x86 leaves
/// MSI-X as `Unsupported`).
pub trait IrqOp: Sync {
    /// Register an interrupt handler for `irq`.
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<IrqHandlerToken>;

    /// Register a primary interrupt handler and a sleepable threaded handler.
    ///
    /// The primary handler still runs in interrupt context and should return
    /// [`IrqEvent::WAKE_THREAD`] or [`IrqEvent::wake_thread_from_sources`] when
    /// the threaded handler should run. Providers that do not support threaded
    /// IRQs return [`ResError::Unsupported`].
    fn request_threaded_irq(
        &self,
        irq: IrqResource,
        primary: Arc<dyn IrqHandler>,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<IrqHandlerToken> {
        let _ = (irq, primary, thread, name);
        Err(ResError::Unsupported)
    }

    /// Register a sleepable threaded handler with the provider's default
    /// primary wake handler.
    ///
    /// This is the common level-triggered device path: the provider is expected
    /// to apply its safe oneshot policy so the line remains masked until the
    /// threaded handler reaches idle.
    fn request_threaded_irq_default(
        &self,
        irq: IrqResource,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<IrqHandlerToken> {
        let _ = (irq, thread, name);
        Err(ResError::Unsupported)
    }

    /// Release an interrupt handler previously registered for `irq`.
    ///
    /// May block while synchronizing in-flight handlers. Call in sleepable
    /// task context without locks or resources needed by the handler. A normal
    /// return must not leave an in-flight handler using resources about to be freed.
    fn release_irq(&self, irq: IrqResource, token: IrqHandlerToken);

    /// Enable or disable delivery of `irq`.
    fn set_irq_enabled(&self, irq: IrqResource, enabled: bool);

    /// Translate a discovered interrupt route into an OS-visible IRQ resource.
    ///
    /// Default: treat the hardware number as the OS-visible number with no
    /// translation.
    fn map_irq(&self, route: IrqRouteDesc) -> ResResult<IrqResource> {
        Ok(IrqResource::new(route.hwirq, route.trigger).with_controller(route.controller))
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

/// RAII handle to a registered interrupt.
///
/// Dropping the handle releases the handler through the provider that created
/// it. Drop may block while synchronizing in-flight handlers and therefore
/// requires sleepable task context without locks needed by the handler.
pub struct Irq {
    provider: Option<&'static dyn IrqOp>,
    resource: IrqResource,
    token: IrqHandlerToken,
    armed: bool,
}

impl Irq {
    /// Register `handler` with an explicit provider.
    pub fn request_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<Self> {
        let token = provider.request_irq(resource, handler)?;
        Ok(Self {
            provider: Some(provider),
            resource,
            token,
            armed: true,
        })
    }

    /// Register primary and threaded handlers with an explicit provider.
    pub fn request_threaded_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        primary: Arc<dyn IrqHandler>,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<Self> {
        let token = provider.request_threaded_irq(resource, primary, thread, name)?;
        Ok(Self {
            provider: Some(provider),
            resource,
            token,
            armed: true,
        })
    }

    /// Register a default-primary threaded handler with an explicit provider.
    pub fn request_threaded_default_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<Self> {
        let token = provider.request_threaded_irq_default(resource, thread, name)?;
        Ok(Self {
            provider: Some(provider),
            resource,
            token,
            armed: true,
        })
    }

    /// Enable or disable delivery of this interrupt.
    pub fn set_enabled(&self, enabled: bool) {
        if let Some(provider) = self.provider {
            provider.set_irq_enabled(self.resource, enabled);
        }
    }

    /// The interrupt number.
    pub fn number(&self) -> usize {
        self.resource.number
    }
}

impl Drop for Irq {
    fn drop(&mut self) {
        if self.armed
            && let Some(provider) = self.provider.take()
        {
            provider.release_irq(self.resource, self.token);
        }
    }
}

impl core::fmt::Debug for Irq {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Irq")
            .field("resource", &self.resource)
            .field("token", &self.token)
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}
