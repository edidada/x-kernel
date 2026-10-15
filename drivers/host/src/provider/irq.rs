// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`driver_base::devres::provider::IrqOp`] contract.

use alloc::sync::Arc;

pub(crate) use driver_base::devres::provider::{
    IrqHandlerToken, IrqOp, IrqResource, IrqRouteDesc, MsiResource,
};
use driver_base::devres::provider::{ResError, ResResult};
// The provider contract (formerly device-res): this module IS the
// definition site now. IRQ vocabulary re-exports from kirq.
pub(crate) use kirq::{IrqController, IrqHandler};
use kirq::{IrqEvent, IrqTrigger};

use super::XKernelResourceProvider;

impl IrqOp for XKernelResourceProvider {
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<IrqHandlerToken> {
        let dispatch_handler: Arc<dyn kirq::IrqHandler> = Arc::new(move |virq| {
            let event = handler.handle(virq);
            event_to_kirq(event)
        });
        match kirq::try_register_shared(irq_resource_to_kirq_spec(irq), dispatch_handler) {
            Ok(Some(token)) => Ok(IrqHandlerToken::shared_action(token.id())),
            Ok(None) => Err(ResError::Busy),
            Err(err) => {
                log::warn!("failed to register IRQ handler for {irq:?}: {err:?}");
                Err(map_irq_desc_error(err))
            }
        }
    }

    fn release_irq(&self, irq: IrqResource, token: IrqHandlerToken) {
        match token {
            IrqHandlerToken::RegularAction => {
                match kirq::try_free_irq(irq_resource_to_kirq_spec(irq)) {
                    Ok(Some(_handler)) => {}
                    Ok(None) => {
                        log::warn!(
                            "IRQ {} regular action was not registered or was not released",
                            irq.number()
                        );
                    }
                    #[expect(
                        clippy::panic,
                        reason = "CG-021-no-panic-on-reachable-failure: TODO(cg): \
                                  IrqOp::release_irq returns (), so a kirq free error such as an \
                                  invalid calling context cannot reach the driver; make \
                                  release_irq fallible and propagate the error through device \
                                  teardown"
                    )]
                    Err(err) => {
                        panic!(
                            "failed to release IRQ {} regular action: {err:?}",
                            irq.number()
                        );
                    }
                }
            }
            IrqHandlerToken::SharedAction(id) => {
                let action_token = kirq::IrqActionToken::new(id);
                match kirq::try_free_irq_action(irq_resource_to_kirq_spec(irq), action_token) {
                    Ok(Some(_handler)) => {}
                    Ok(None) => {
                        log::warn!(
                            "IRQ {} action token {} was not registered or was not released",
                            irq.number(),
                            id
                        );
                    }
                    #[expect(
                        clippy::panic,
                        reason = "CG-021-no-panic-on-reachable-failure: TODO(cg): \
                                  IrqOp::release_irq returns (), so a kirq free error such as an \
                                  invalid calling context cannot reach the driver; make \
                                  release_irq fallible and propagate the error through device \
                                  teardown"
                    )]
                    Err(err) => {
                        panic!(
                            "failed to release IRQ {} action token {}: {err:?}",
                            irq.number(),
                            id
                        );
                    }
                }
            }
        }
    }

    fn set_irq_enabled(&self, irq: IrqResource, enabled: bool) {
        kirq::enable(irq_resource_to_kirq_spec(irq), enabled);
    }

    fn map_irq(&self, route: IrqRouteDesc) -> ResResult<IrqResource> {
        let domain = effective_route_domain(route.controller, route.domain);
        let desc = irq_route_to_kirq_desc(route);
        let virq = kirq::try_map(desc).map_err(|err| {
            log::warn!("failed to map IRQ route {route:?}: {err:?}");
            map_irq_desc_error(err)
        })?;
        Ok(IrqResource::mapped(
            virq,
            route.trigger,
            route.controller,
            domain,
            route.hwirq,
        ))
    }

    fn alloc_msix(&self) -> ResResult<MsiResource> {
        #[cfg(target_arch = "x86_64")]
        {
            let allocation = kirq::alloc_msix(kirq::IrqAffinity::Any).ok_or(ResError::NoMemory)?;
            let irq = IrqResource::plain(allocation.virq(), IrqTrigger::EdgeRising);
            let message = allocation.message();
            Ok(MsiResource::new(
                irq,
                driver_base::devres::provider::MsiMessage::new(message.address(), message.data()),
            ))
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            Err(ResError::Unsupported)
        }
    }

    fn free_msix(&self, resource: MsiResource) {
        #[cfg(target_arch = "x86_64")]
        {
            if !kirq::free_msix(resource.irq.number()) {
                log::warn!(
                    "failed to free MSI-X IRQ {} (not allocated?)",
                    resource.irq.number()
                );
            }
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = resource;
        }
    }
}

/// Assemble a provider [`IrqRouteDesc`] from a driver-core firmware route.
pub(crate) fn to_runtime_route(
    controller: driver_base::IrqController,
    domain: Option<kirq::IrqDomainId>,
    hwirq: usize,
    trigger: kirq::IrqTrigger,
) -> IrqRouteDesc {
    IrqRouteDesc {
        hwirq,
        trigger,
        controller: to_runtime_controller(controller),
        domain,
    }
}

/// driver-core controller vocabulary → the runtime provider's (kirq).
pub(crate) fn to_runtime_controller(controller: driver_base::IrqController) -> kirq::IrqController {
    use driver_base::IrqController as C;
    match controller {
        C::Gic => kirq::IrqController::Gic,
        C::Plic => kirq::IrqController::Plic,
        C::IoApic => kirq::IrqController::IoApic,
        C::LoongArchExtioi => kirq::IrqController::LoongArchExtioi,
        C::Msi | C::Unknown => kirq::IrqController::Unknown,
    }
}

/// Resolve a descriptor IRQ resource to the OS-visible virtual irq WITHOUT
/// registering a handler.
///
/// A firmware route (hwirq + trigger + controller, captured once at
/// discovery) maps through its controller's irq domain; an already-resolved
/// virq passes through unchanged. This is the provider-side half of
/// [`driver_base::DeviceResourceServices::map_irq`] for callers holding a
/// bare [`driver_base::IrqSpec`] instead of a service handle — today only
/// the virtio transport probe, which resolves the descriptor's legacy INTx
/// route as the fallback for a transport-bound MSI-X vector.
///
/// Lives here, not in the virtio family library: the controller-vocabulary
/// translation and the domain fallback are provider knowledge, and a driver
/// crate must not carry them (see
/// `docs/ai/driver-subsystem-boundaries.md`, rule L1).
///
/// Returns `None` when the route cannot be mapped (no domain registered for
/// the controller, or the hwirq is already claimed).
pub(crate) fn resolve_irq_spec(
    provider: &XKernelResourceProvider,
    spec: &driver_base::IrqSpec,
) -> Option<usize> {
    match *spec {
        driver_base::IrqSpec::ResolvedVirq { number, .. } => Some(number),
        driver_base::IrqSpec::FirmwareRoute {
            controller,
            domain,
            hwirq,
            trigger,
        } => {
            let route = to_runtime_route(controller, domain, hwirq, trigger);
            provider.map_irq(route).ok().map(|irq| irq.number())
        }
    }
}

/// The effective domain a driver-core firmware route maps through:
/// firmware-stated when present, else the controller's root domain. The
/// result is recorded on the mapped resource — the identity never
/// diverges between request-time mapping and release.
pub(crate) fn effective_domain(
    controller: driver_base::IrqController,
    stated: Option<kirq::IrqDomainId>,
) -> Option<kirq::IrqDomainId> {
    effective_route_domain(to_runtime_controller(controller), stated)
}

fn dev_irq_controller_to_kirq_domain(controller: kirq::IrqController) -> Option<kirq::IrqDomainId> {
    // The provider's `IrqDomainId` vocabulary is provider-local. Only
    // controllers with a registered kirq domain can produce
    // data-plane-resolvable mappings here.
    match controller {
        IrqController::Gic => Some(kirq::GIC_ROOT_DOMAIN),
        IrqController::Plic => Some(kirq::PLIC_ROOT_DOMAIN),
        IrqController::IoApic => Some(kirq::IO_APIC_DOMAIN),
        // Msi carries its own domain in kirq; the provider contract has no
        // provider-registered Msi domain to name.
        IrqController::Msi | IrqController::LoongArchExtioi | IrqController::Unknown => None,
    }
}

/// The domain a route maps through: firmware-stated when present, else the
/// controller's root domain (single-domain controllers — today's GIC/PLIC
/// roots). The SAME effective domain is recorded on the mapped resource, so
/// the identity never diverges between mapping and later release.
fn effective_route_domain(
    controller: kirq::IrqController,
    stated: Option<kirq::IrqDomainId>,
) -> Option<kirq::IrqDomainId> {
    stated.or_else(|| dev_irq_controller_to_kirq_domain(controller))
}

fn irq_route_to_kirq_desc(route: IrqRouteDesc) -> kirq::IrqDesc {
    let mut desc = kirq::IrqDesc::new(route.hwirq, route.trigger).with_controller(route.controller);
    if let Some(domain) = effective_route_domain(route.controller, route.domain) {
        desc = desc.with_domain(domain);
    }
    desc
}

fn irq_resource_to_kirq_spec(irq: IrqResource) -> kirq::IrqSpec {
    match irq {
        IrqResource::PlainVirq { number, .. } => kirq::IrqSpec::PlainVirq(number),
        IrqResource::DomainMapped {
            number,
            trigger,
            controller,
            domain,
            hwirq,
        } => {
            let mut desc = kirq::IrqDesc::new(hwirq, trigger).with_controller(controller);
            // Pin the logical irq when one is stated (the request paths
            // resolve firmware routes through map_irq first, so a
            // DomainMapped resource carries its real virq here; a 0 can
            // only come from a bespoke provider and means "allocate
            // fresh").
            if number != 0 {
                desc = desc.with_virq(number);
            }
            // The origin's domain is authoritative — no controller-based
            // re-derivation here (that guess is what used to drop
            // firmware-stated domains).
            if let Some(domain) = domain {
                desc = desc.with_domain(domain);
            }
            kirq::IrqSpec::Desc(desc)
        }
    }
}

fn event_to_kirq(event: kirq::IrqEvent) -> kirq::IrqEvent {
    if event.handled() {
        kirq::IrqEvent::from_sources(event.sources())
    } else {
        kirq::IrqEvent::NOT_HANDLED
    }
}

fn map_irq_desc_error(err: kirq::IrqDescError) -> ResError {
    match err {
        kirq::IrqDescError::VirqExhausted { .. } => ResError::NoMemory,
        kirq::IrqDescError::HwirqConflict { .. }
        | kirq::IrqDescError::DomainConflict { .. }
        | kirq::IrqDescError::VirqConflict { .. }
        | kirq::IrqDescError::MappingConflict { .. }
        | kirq::IrqDescError::VirqMappingConflict { .. }
        | kirq::IrqDescError::UnknownDomain { .. }
        | kirq::IrqDescError::UnknownIrq
        | kirq::IrqDescError::TeardownInProgress { .. }
        | kirq::IrqDescError::NoIrqAction { .. }
        | kirq::IrqDescError::InvalidContext { .. }
        | kirq::IrqDescError::SyncWaitFailed { .. } => ResError::InvalidResource,
    }
}

#[unittest::mod_test]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_plain_irq_resource_maps_to_plain_virq_spec() {
        let resource = IrqResource::plain(32, IrqTrigger::Unknown(0));

        let spec = irq_resource_to_kirq_spec(resource);

        assert_eq!(spec, kirq::IrqSpec::PlainVirq(32));
    }

    #[def_test]
    fn test_routed_irq_resource_maps_to_descriptor_spec() {
        let resource = IrqResource::mapped(
            48,
            IrqTrigger::LevelHigh,
            IrqController::Gic,
            Some(kirq::GIC_ROOT_DOMAIN),
            30,
        );

        let spec = irq_resource_to_kirq_spec(resource);
        let kirq::IrqSpec::Desc(desc) = spec else {
            panic!("routed IRQ resource should map to descriptor spec");
        };

        assert_eq!(desc.logical_irq(), Some(48));
        assert_eq!(desc.hwirq, 30);
        assert_eq!(desc.domain, Some(kirq::GIC_ROOT_DOMAIN));
    }

    /// Regression: a firmware-STATED domain is authoritative. The old
    /// field-based resource re-derived the domain from the controller on
    /// every conversion — a second GIC instance (or any non-root domain)
    /// silently lost its identity between request and release.
    #[def_test]
    fn test_stated_domain_is_not_re_derived_from_controller() {
        let stated = kirq::IrqDomainId::new(7); // a non-root domain id
        let resource = IrqResource::mapped(
            0,
            IrqTrigger::LevelHigh,
            IrqController::Gic,
            Some(stated),
            30,
        );

        let spec = irq_resource_to_kirq_spec(resource);
        let kirq::IrqSpec::Desc(desc) = spec else {
            panic!("routed IRQ resource should map to descriptor spec");
        };

        assert_eq!(desc.domain, Some(stated));
        assert_ne!(desc.domain, Some(kirq::GIC_ROOT_DOMAIN));
        // Pre-mapping (number 0): no stale preferred virq is pinned.
        assert_eq!(desc.logical_irq(), None);
    }

    /// `effective_domain` derives the controller's root domain only when
    /// firmware stated none — the single-domain-controller fallback.
    #[def_test]
    fn test_effective_domain_falls_back_to_controller_root() {
        assert_eq!(
            effective_domain(IrqController::Gic, None),
            Some(kirq::GIC_ROOT_DOMAIN)
        );
        let stated = kirq::IrqDomainId::new(9);
        assert_eq!(
            effective_domain(IrqController::Gic, Some(stated)),
            Some(stated)
        );
    }

    #[def_test]
    fn test_device_irq_event_wake_request_maps_to_handled_until_threadirq_is_available() {
        let event = event_to_kirq(IrqEvent::from_sources(0b1010));

        assert!(event.handled());
        assert_eq!(event.sources(), 0b1010);
    }

    #[def_test]
    fn test_device_irq_event_handled_does_not_wake_thread() {
        let event = event_to_kirq(IrqEvent::from_sources(0b0101));

        assert!(event.handled());
        assert_eq!(event.sources(), 0b0101);
    }
}
