// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`driver_core::provider::IrqOp`] contract.

use alloc::sync::Arc;

pub use driver_core::provider::{IrqHandlerToken, IrqOp, IrqResource, IrqRouteDesc, MsiResource};
use driver_core::provider::{ResError, ResResult};
// The provider contract (formerly device-res): this module IS the
// definition site now. IRQ vocabulary re-exports from kirq.
pub use kirq::{IrqController, IrqEvent, IrqHandler, IrqTrigger};

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
                            irq.number
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
                            irq.number
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
                            irq.number,
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
                            irq.number, id
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
        let desc = irq_route_to_kirq_desc(route);
        let virq = kirq::try_map(desc).map_err(|err| {
            log::warn!("failed to map IRQ route {route:?}: {err:?}");
            map_irq_desc_error(err)
        })?;
        Ok(IrqResource::new(virq, route.trigger)
            .with_controller(route.controller)
            .with_hwirq(route.hwirq))
    }

    fn alloc_msix(&self) -> ResResult<MsiResource> {
        #[cfg(target_arch = "x86_64")]
        {
            let allocation = kirq::alloc_msix(kirq::IrqAffinity::Any).ok_or(ResError::NoMemory)?;
            let irq = IrqResource::new(allocation.virq(), IrqTrigger::EdgeRising)
                .with_controller(IrqController::Unknown);
            let message = allocation.message();
            Ok(MsiResource::new(
                irq,
                driver_core::provider::MsiMessage::new(message.address(), message.data()),
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
            if !kirq::free_msix(resource.irq.number) {
                log::warn!(
                    "failed to free MSI-X IRQ {} (not allocated?)",
                    resource.irq.number
                );
            }
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = resource;
        }
    }
}

fn dev_irq_controller_to_kirq_domain(controller: kirq::IrqController) -> Option<kirq::IrqDomainId> {
    // `device_res::IrqDomainId` is provider-local. Only controllers with a
    // registered kirq domain can produce data-plane-resolvable mappings here.
    match controller {
        IrqController::Gic => Some(kirq::GIC_ROOT_DOMAIN),
        IrqController::Plic => Some(kirq::PLIC_ROOT_DOMAIN),
        IrqController::IoApic => Some(kirq::IO_APIC_DOMAIN),
        // Msi carries its own domain in kirq; the device-res side has no
        // provider-registered Msi domain to name.
        IrqController::Msi | IrqController::LoongArchExtioi | IrqController::Unknown => None,
    }
}

fn irq_route_to_kirq_desc(route: IrqRouteDesc) -> kirq::IrqDesc {
    let mut desc = kirq::IrqDesc::new(route.hwirq, route.trigger).with_controller(route.controller);
    if let Some(domain) = dev_irq_controller_to_kirq_domain(route.controller) {
        desc = desc.with_domain(domain);
    }
    desc
}

fn irq_resource_to_kirq_spec(irq: IrqResource) -> kirq::IrqSpec {
    if irq.hwirq.is_none()
        && irq.domain.is_none()
        && irq.controller.unwrap_or(IrqController::Unknown) == IrqController::Unknown
    {
        return kirq::IrqSpec::PlainVirq(irq.number);
    }

    let hwirq = irq.hwirq.unwrap_or(irq.number);
    let controller = irq.controller.unwrap_or(IrqController::Unknown);
    let mut desc = kirq::IrqDesc::new(hwirq, irq.trigger).with_controller(controller);
    // Only pin the logical irq when the caller actually resolved one
    // (number != 0). A firmware route (number 0 + hwirq + controller) maps
    // through the controller's domain: an existing mapping is reused, a
    // fresh dynamic virq is allocated — never a collision with a stale
    // "preferred" number.
    if irq.number != 0 {
        desc = desc.with_virq(irq.number);
    }
    if let Some(domain) = dev_irq_controller_to_kirq_domain(controller) {
        desc = desc.with_domain(domain);
    }
    kirq::IrqSpec::Desc(desc)
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
        let resource = IrqResource::new(32, IrqTrigger::Unknown(0));

        let spec = irq_resource_to_kirq_spec(resource);

        assert_eq!(spec, kirq::IrqSpec::PlainVirq(32));
    }

    #[def_test]
    fn test_routed_irq_resource_maps_to_descriptor_spec() {
        let resource = IrqResource::new(48, IrqTrigger::LevelHigh)
            .with_controller(IrqController::Gic)
            .with_hwirq(30);

        let spec = irq_resource_to_kirq_spec(resource);
        let kirq::IrqSpec::Desc(desc) = spec else {
            panic!("routed IRQ resource should map to descriptor spec");
        };

        assert_eq!(desc.logical_irq(), Some(48));
        assert_eq!(desc.hwirq, 30);
        assert_eq!(desc.domain, Some(kirq::GIC_ROOT_DOMAIN));
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
