// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`device_res::IrqOp`] provider contract.

use alloc::sync::Arc;

use device_res::{
    IrqController as DevIrqController, IrqEvent as DevIrqEvent, IrqHandler as DevIrqHandler,
    IrqHandlerToken, IrqOp, IrqResource, IrqRouteDesc, IrqTrigger as DevIrqTrigger, MsiResource,
    ResError, ResResult,
};

use crate::XKernelResourceProvider;

impl IrqOp for XKernelResourceProvider {
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn DevIrqHandler>,
    ) -> ResResult<IrqHandlerToken> {
        let dispatch_handler: Arc<dyn kirq::IrqHandler> = Arc::new(move |virq| {
            let event = handler.handle(virq);
            dev_irq_event_to_kirq(event)
        });
        let token =
            crate::native_irq::register_shared(irq_resource_to_kirq_spec(irq), dispatch_handler)?;
        Ok(IrqHandlerToken::shared_action(token.id()))
    }

    fn release_irq(&self, irq: IrqResource, token: IrqHandlerToken) {
        let spec = irq_resource_to_kirq_spec(irq);
        let result = match token {
            IrqHandlerToken::SharedAction(id) => crate::native_irq::release_shared(spec, id),
            IrqHandlerToken::RegularAction => kirq::try_free_irq(spec)
                .map_err(map_irq_desc_error)
                .and_then(|handler| handler.map(drop).ok_or(ResError::NotFound)),
        };
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: TODO(cg): infallible IRQ release \
                      cannot authorize reclamation after rejected detach/drain; fallible teardown \
                      must retain owners"
        )]
        if let Err(error) = result {
            panic!("IRQ release did not establish quiescence: {irq:?} {token:?}: {error:?}");
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
        let resource = crate::msi::allocate()?;
        Ok(MsiResource::new(
            IrqResource::new(resource.irq.virq().raw(), DevIrqTrigger::EdgeRising)
                .with_controller(DevIrqController::Unknown),
            device_res::MsiMessage::new(resource.message.address, resource.message.data),
        ))
    }

    fn free_msix(&self, resource: MsiResource) {
        crate::msi::release(resource.irq.number);
    }
}

fn dev_irq_trigger_to_kirq(trigger: DevIrqTrigger) -> kirq::IrqTrigger {
    match trigger {
        DevIrqTrigger::EdgeRising => kirq::IrqTrigger::EdgeRising,
        DevIrqTrigger::EdgeFalling => kirq::IrqTrigger::EdgeFalling,
        DevIrqTrigger::LevelHigh => kirq::IrqTrigger::LevelHigh,
        DevIrqTrigger::LevelLow => kirq::IrqTrigger::LevelLow,
        DevIrqTrigger::Unknown(flags) => kirq::IrqTrigger::Unknown(flags),
    }
}

fn dev_irq_controller_to_kirq(controller: DevIrqController) -> kirq::IrqController {
    match controller {
        DevIrqController::Gic => kirq::IrqController::Gic,
        DevIrqController::Plic => kirq::IrqController::Plic,
        DevIrqController::IoApic => kirq::IrqController::IoApic,
        DevIrqController::LoongArchExtioi => kirq::IrqController::LoongArchExtioi,
        DevIrqController::Unknown => kirq::IrqController::Unknown,
    }
}

fn dev_irq_controller_to_kirq_domain(controller: DevIrqController) -> Option<kirq::IrqDomainId> {
    // `device_res::IrqDomainId` is provider-local. Only controllers with a
    // registered kirq domain can produce data-plane-resolvable mappings here.
    match controller {
        DevIrqController::Gic => Some(kirq::GIC_ROOT_DOMAIN),
        DevIrqController::Plic => Some(kirq::PLIC_ROOT_DOMAIN),
        DevIrqController::IoApic => Some(kirq::IO_APIC_DOMAIN),
        DevIrqController::LoongArchExtioi | DevIrqController::Unknown => None,
    }
}

fn irq_route_to_kirq_desc(route: IrqRouteDesc) -> kirq::IrqDesc {
    let mut desc = kirq::IrqDesc::new(route.hwirq, dev_irq_trigger_to_kirq(route.trigger))
        .with_controller(dev_irq_controller_to_kirq(route.controller));
    if let Some(domain) = dev_irq_controller_to_kirq_domain(route.controller) {
        desc = desc.with_domain(domain);
    }
    desc
}

fn irq_resource_to_kirq_spec(irq: IrqResource) -> kirq::IrqSpec {
    if irq.hwirq.is_none()
        && irq.domain.is_none()
        && irq.controller.unwrap_or(DevIrqController::Unknown) == DevIrqController::Unknown
    {
        return kirq::IrqSpec::PlainVirq(irq.number);
    }

    let hwirq = irq.hwirq.unwrap_or(irq.number);
    let controller = irq.controller.unwrap_or(DevIrqController::Unknown);
    let mut desc = kirq::IrqDesc::new(hwirq, dev_irq_trigger_to_kirq(irq.trigger))
        .with_controller(dev_irq_controller_to_kirq(controller))
        .with_virq(irq.number);
    if let Some(domain) = dev_irq_controller_to_kirq_domain(controller) {
        desc = desc.with_domain(domain);
    }
    kirq::IrqSpec::Desc(desc)
}

fn dev_irq_event_to_kirq(event: DevIrqEvent) -> kirq::IrqEvent {
    if event.handled() {
        kirq::IrqEvent::from_sources(event.sources())
    } else {
        kirq::IrqEvent::NOT_HANDLED
    }
}

fn map_irq_desc_error(err: kirq::IrqDescError) -> ResError {
    match err {
        kirq::IrqDescError::VirqExhausted { .. } | kirq::IrqDescError::NoMemory => {
            ResError::NoMemory
        }
        kirq::IrqDescError::InvalidHwirq { .. }
        | kirq::IrqDescError::UnsupportedFlow { .. }
        | kirq::IrqDescError::ConfigurationFailed { .. }
        | kirq::IrqDescError::HwirqConflict { .. }
        | kirq::IrqDescError::DomainConflict { .. }
        | kirq::IrqDescError::VirqConflict { .. }
        | kirq::IrqDescError::MappingConflict { .. }
        | kirq::IrqDescError::VirqMappingConflict { .. }
        | kirq::IrqDescError::UnknownDomain { .. }
        | kirq::IrqDescError::UnknownIrq
        | kirq::IrqDescError::TeardownInProgress { .. }
        | kirq::IrqDescError::NoIrqAction { .. }
        | kirq::IrqDescError::InvalidContext { .. } => ResError::InvalidResource,
    }
}

unittest::path_mod_test!(tests, "../ut-tests/irq.rs");
