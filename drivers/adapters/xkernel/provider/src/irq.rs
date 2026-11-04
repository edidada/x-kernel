// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`device_res::IrqOp`] provider contract.

use alloc::sync::Arc;

use device_res::{
    IrqController as DevIrqController, IrqHandler as DevIrqHandler, IrqHandlerToken, IrqOp,
    IrqResource, IrqRouteDesc, IrqTrigger as DevIrqTrigger, MsiResource, ResError, ResResult,
};

use crate::XKernelResourceProvider;

impl IrqOp for XKernelResourceProvider {
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn DevIrqHandler>,
    ) -> ResResult<IrqHandlerToken> {
        let dispatch_handler = crate::native_irq::adapt_handler(handler);
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
        map_route_with(route, kirq::try_map)
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

fn default_domain(controller: DevIrqController) -> Option<kirq::IrqDomainId> {
    match controller {
        DevIrqController::Gic => Some(kirq::GIC_ROOT_DOMAIN),
        DevIrqController::Plic => Some(kirq::PLIC_ROOT_DOMAIN),
        DevIrqController::IoApic => Some(kirq::IO_APIC_DOMAIN),
        DevIrqController::LoongArchExtioi | DevIrqController::Unknown | DevIrqController::Msi => {
            None
        }
    }
}

fn irq_route_to_kirq_desc(route: IrqRouteDesc) -> kirq::IrqDesc {
    let mut desc = kirq::IrqDesc::new(route.hwirq, route.trigger).with_controller(route.controller);
    if let Some(domain) = route.domain.or_else(|| default_domain(route.controller)) {
        desc = desc.with_domain(domain);
    }
    desc
}

fn map_route_with(
    route: IrqRouteDesc,
    map: impl FnOnce(kirq::IrqDesc) -> Result<usize, kirq::IrqDescError>,
) -> ResResult<IrqResource> {
    let desc = irq_route_to_kirq_desc(route);
    let virq = map(desc).map_err(|err| {
        log::warn!("failed to map IRQ route {route:?}: {err:?}");
        map_irq_desc_error(err)
    })?;
    let mut resource = IrqResource::new(virq, route.trigger)
        .with_controller(route.controller)
        .with_hwirq(route.hwirq);
    resource.domain = desc.domain;
    Ok(resource)
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
    let mut desc = kirq::IrqDesc::new(hwirq, irq.trigger)
        .with_controller(controller)
        .with_virq(irq.number);
    if let Some(domain) = irq.domain.or_else(|| default_domain(controller)) {
        desc = desc.with_domain(domain);
    }
    kirq::IrqSpec::Desc(desc)
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
