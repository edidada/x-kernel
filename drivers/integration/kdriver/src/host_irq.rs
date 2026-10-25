// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native ordinary IRQ capability; no boot installation or private IRQ registry.

use alloc::sync::Arc;

use driver_os_api::{
    IrqAction, IrqController, IrqHandlerFn, IrqHandlerToken, IrqOp, IrqResource, IrqRouteDesc,
    ResError, ResResult, Virq,
};

/// Native shared, ordinary hardirq provider backed by kirq.
///
/// Requests require sleepable task context and return `BadState` otherwise.
/// The first shared action uses kirq's automatic line enable policy. Callbacks
/// must not sleep; this provider supplies neither threaded nor pseudo-NMI IRQs.
/// Plain virqs must already have a descriptor; a specified trigger must match
/// it. Unknown trigger means no new requirement, not a request to reconfigure.
/// The caller must keep the resolved mapping stable through registration and
/// release; the descriptor preflight is not an atomic mapping-lifetime guard.
///
/// Release must receive the exact resource/token returned for one registration,
/// exactly once, in sleepable task context with no locks needed by the handler.
/// Hardware must already be stopped by the driver. A normal release return
/// guarantees synchronous removal and drain of that action. Contract violations
/// fail closed by panicking: the infallible release API cannot authorize resource
/// reclamation after an unsuccessful drain. Construction installs nothing.
pub struct NativeIrq;

impl IrqOp for NativeIrq {
    fn map_irq(&self, route: IrqRouteDesc) -> ResResult<IrqResource> {
        map_route_with(route, kirq::try_map)
    }

    fn request_irq(&self, irq: IrqResource, handler: IrqHandlerFn) -> ResResult<IrqHandlerToken> {
        request_with(
            irq,
            handler,
            ktask::is_sleepable_context(),
            kirq::descriptor,
            kirq::try_register_shared,
        )
    }

    fn release_irq(&self, irq: IrqResource, token: IrqHandlerToken) {
        match release_with(irq, token, kirq::try_free_irq_action) {
            Ok(()) => {}
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: TODO(cg): IrqOp release returns \
                          (); invalid context or an unmatched action cannot authorize \
                          reclamation; a future fallible teardown contract must retain driver \
                          resources on error"
            )]
            Err(error) => {
                panic!(
                    "native IRQ release did not establish quiescence: {irq:?} {token:?}: {error:?}"
                );
            }
        }
    }

    fn set_irq_enabled(&self, irq: IrqResource, enabled: bool) {
        kirq::enable(resource_spec(irq), enabled);
    }
}

fn route_desc(route: IrqRouteDesc) -> ResResult<kirq::IrqDesc> {
    let domain = match route.domain {
        Some(domain) => domain,
        None => match route.controller {
            IrqController::Gic => kirq::GIC_ROOT_DOMAIN,
            IrqController::Plic => kirq::PLIC_ROOT_DOMAIN,
            IrqController::IoApic => kirq::IO_APIC_DOMAIN,
            IrqController::LoongArchExtioi | IrqController::Unknown | IrqController::Msi => {
                return Err(ResError::Unsupported);
            }
        },
    };
    Ok(kirq::IrqDesc::new(route.hwirq, route.trigger)
        .with_controller(route.controller)
        .with_domain(domain))
}

fn map_route_with(
    route: IrqRouteDesc,
    map: impl FnOnce(kirq::IrqDesc) -> Result<usize, kirq::IrqDescError>,
) -> ResResult<IrqResource> {
    let desc = route_desc(route)?;
    let virq = map(desc).map_err(map_error)?;
    Ok(IrqResource::mapped(
        Virq::new(virq),
        desc.trigger,
        desc.controller,
        desc.domain,
        desc.hwirq,
    ))
}

fn resource_spec(irq: IrqResource) -> kirq::IrqSpec {
    match irq {
        IrqResource::PlainVirq { virq, .. } => kirq::IrqSpec::PlainVirq(virq.raw()),
        IrqResource::DomainMapped {
            virq,
            trigger,
            controller,
            domain,
            hwirq,
        } => {
            let mut desc = kirq::IrqDesc::new(hwirq, trigger)
                .with_controller(controller)
                .with_virq(virq.raw());
            desc.domain = domain;
            kirq::IrqSpec::Desc(desc)
        }
    }
}

fn adapt_handler(handler: IrqHandlerFn) -> kirq::Handler {
    Arc::new(move |_| match handler() {
        IrqAction::Handled => kirq::IrqEvent::HANDLED,
        IrqAction::NotHandled => kirq::IrqEvent::NOT_HANDLED,
    })
}

fn request_with(
    irq: IrqResource,
    handler: IrqHandlerFn,
    is_sleepable: bool,
    lookup: impl FnOnce(usize) -> Option<kirq::IrqDesc>,
    register: impl FnOnce(
        kirq::IrqSpec,
        kirq::Handler,
    ) -> Result<Option<kirq::IrqActionToken>, kirq::IrqDescError>,
) -> ResResult<IrqHandlerToken> {
    if !is_sleepable {
        return Err(ResError::BadState);
    }
    if let IrqResource::PlainVirq { virq, trigger } = irq {
        let desc = lookup(virq.raw()).ok_or(ResError::InvalidResource)?;
        if !matches!(trigger, driver_os_api::IrqTrigger::Unknown(_)) && trigger != desc.trigger {
            return Err(ResError::InvalidResource);
        }
    }
    let token = register(resource_spec(irq), adapt_handler(handler))
        .map_err(map_error)?
        .ok_or(ResError::Busy)?;
    Ok(IrqHandlerToken::SharedAction(token.id()))
}

fn release_with(
    irq: IrqResource,
    token: IrqHandlerToken,
    free: impl FnOnce(
        kirq::IrqSpec,
        kirq::IrqActionToken,
    ) -> Result<Option<kirq::Handler>, kirq::IrqDescError>,
) -> ResResult {
    let IrqHandlerToken::SharedAction(id) = token else {
        return Err(ResError::InvalidResource);
    };
    let handler = free(resource_spec(irq), kirq::IrqActionToken::new(id))
        .map_err(map_error)?
        .ok_or(ResError::NotFound)?;
    drop(handler);
    Ok(())
}

fn map_error(error: kirq::IrqDescError) -> ResError {
    match error {
        kirq::IrqDescError::VirqExhausted { .. } => ResError::NoMemory,
        kirq::IrqDescError::InvalidContext { .. } => ResError::BadState,
        kirq::IrqDescError::TeardownInProgress { .. } => ResError::Busy,
        kirq::IrqDescError::HwirqConflict { .. }
        | kirq::IrqDescError::DomainConflict { .. }
        | kirq::IrqDescError::VirqConflict { .. }
        | kirq::IrqDescError::MappingConflict { .. }
        | kirq::IrqDescError::VirqMappingConflict { .. }
        | kirq::IrqDescError::UnknownDomain { .. }
        | kirq::IrqDescError::UnknownIrq
        | kirq::IrqDescError::NoIrqAction { .. } => ResError::InvalidResource,
    }
}

unittest::path_mod_test!(tests, "../ut-tests/host_irq.rs");
