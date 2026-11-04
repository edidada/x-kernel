// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use driver_os_api::{IrqDomainId, IrqTrigger};
use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn shared_backend_preserves_legacy_sources_and_rejects_missing_release() {
    let token = register_shared_with(
        kirq::IrqSpec::PlainVirq(903),
        Arc::new(|_| kirq::IrqEvent::from_sources(0b1010)),
        |_, handler| {
            core::assert_eq!(handler.handle(903).sources(), 0b1010);
            Ok(Some(kirq::IrqActionToken::new(73)))
        },
    )
    .unwrap();
    assert_eq!(token.id(), 73);
    assert_eq!(
        release_shared_with(kirq::IrqSpec::PlainVirq(903), token.id(), |_, _| Ok(None)),
        Err(ResError::NotFound)
    );
}

#[def_test]
fn native_irq_mapping_preserves_explicit_domain_and_nonidentity_virq() {
    let domain = IrqDomainId::new(0x7051);
    let route = IrqRouteDesc {
        hwirq: 37,
        trigger: IrqTrigger::LevelHigh,
        controller: IrqController::Gic,
        domain: Some(domain),
    };
    let mut observed = None;
    let resource = map_route_with(route, |desc| {
        observed = Some(desc);
        Ok(903)
    })
    .unwrap();
    let expected = kirq::IrqDesc::new(37, IrqTrigger::LevelHigh)
        .with_controller(IrqController::Gic)
        .with_domain(domain);
    assert_eq!(observed, Some(expected));
    assert_eq!(
        resource,
        IrqResource::mapped(
            Virq::new(903),
            route.trigger,
            route.controller,
            Some(domain),
            37
        )
    );
    assert_eq!(
        resource_spec(resource),
        kirq::IrqSpec::Desc(expected.with_virq(903))
    );
}

#[def_test]
fn native_irq_defaults_only_missing_domains_and_preserves_plain_virq() {
    for (controller, domain) in [
        (IrqController::Gic, kirq::GIC_ROOT_DOMAIN),
        (IrqController::Plic, kirq::PLIC_ROOT_DOMAIN),
        (IrqController::IoApic, kirq::IO_APIC_DOMAIN),
    ] {
        let desc = route_desc(IrqRouteDesc {
            controller,
            domain: None,
            hwirq: 19,
            trigger: IrqTrigger::EdgeRising,
        })
        .unwrap();
        assert_eq!(desc.domain, Some(domain));
    }
    for controller in [
        IrqController::Unknown,
        IrqController::LoongArchExtioi,
        IrqController::Msi,
    ] {
        let mut called = false;
        let result = map_route_with(
            IrqRouteDesc {
                controller,
                domain: None,
                hwirq: 19,
                trigger: IrqTrigger::LevelHigh,
            },
            |_| {
                called = true;
                Ok(19)
            },
        );
        assert_eq!(result, Err(ResError::Unsupported));
        assert_eq!(called, false);
    }
    let plain = IrqResource::plain(Virq::new(903), IrqTrigger::LevelHigh);
    assert_eq!(resource_spec(plain), kirq::IrqSpec::PlainVirq(903));
}

#[def_test]
fn native_irq_request_and_release_keep_exact_action_identity() {
    let resource = IrqResource::mapped(
        Virq::new(903),
        IrqTrigger::LevelHigh,
        IrqController::Gic,
        Some(IrqDomainId::new(0x7051)),
        37,
    );
    for action in [IrqAction::Handled, IrqAction::NotHandled] {
        let mut registered = None;
        let token = request_with(
            resource,
            Arc::new(move || action),
            true,
            |_| None,
            |spec, handler| {
                registered = Some((spec, handler.handle(903)));
                Ok(Some(kirq::IrqActionToken::new(73)))
            },
        )
        .unwrap();
        let mut released = None;
        let result = release_with(resource, token, |spec, token| {
            released = Some((spec, token.id()));
            Ok(Some(adapt_handler(Arc::new(|| IrqAction::NotHandled))))
        });
        assert_eq!(result, Ok(()));
        assert_eq!(token, IrqHandlerToken::SharedAction(73));
        let event = registered.unwrap();
        assert_eq!(event.0, resource_spec(resource));
        assert_eq!(event.1.handled(), action == IrqAction::Handled);
        assert_eq!(event.1.sources(), 0);
        assert_eq!(released, Some((resource_spec(resource), 73)));
    }
}

#[def_test]
fn native_irq_invalid_context_rejects_before_registration() {
    let mut called = false;
    let result = request_with(
        IrqResource::plain(Virq::new(903), IrqTrigger::LevelHigh),
        Arc::new(|| IrqAction::Handled),
        false,
        |_| None,
        |_, _| {
            called = true;
            Ok(Some(kirq::IrqActionToken::new(73)))
        },
    );
    assert_eq!(result, Err(ResError::BadState));
    assert_eq!(called, false);
}

#[def_test]
fn native_irq_failures_never_report_successful_registration_or_release() {
    let resource = IrqResource::mapped(
        Virq::new(903),
        IrqTrigger::LevelHigh,
        IrqController::Gic,
        Some(IrqDomainId::new(0x7051)),
        37,
    );
    let token = IrqHandlerToken::SharedAction(73);
    let result = request_with(
        resource,
        Arc::new(|| IrqAction::Handled),
        true,
        |_| None,
        |_, _| Ok(None),
    );
    assert_eq!(result, Err(ResError::Busy));
    let result = request_with(
        resource,
        Arc::new(|| IrqAction::Handled),
        true,
        |_| None,
        |_, _| Err(kirq::IrqDescError::TeardownInProgress { virq: 903 }),
    );
    assert_eq!(result, Err(ResError::Busy));
    assert_eq!(
        release_with(resource, token, |_, _| Ok(None)),
        Err(ResError::NotFound)
    );
    let result = release_with(resource, token, |_, _| {
        Err(kirq::IrqDescError::InvalidContext {
            operation: "free_irq",
        })
    });
    assert_eq!(result, Err(ResError::BadState));
    let mut called = false;
    let result = release_with(resource, IrqHandlerToken::RegularAction, |_, _| {
        called = true;
        Ok(None)
    });
    assert_eq!(result, Err(ResError::InvalidResource));
    assert_eq!(called, false);
    let result = map_route_with(
        IrqRouteDesc {
            controller: IrqController::Gic,
            domain: Some(IrqDomainId::new(0x7051)),
            hwirq: 37,
            trigger: IrqTrigger::LevelHigh,
        },
        |_| Err(kirq::IrqDescError::VirqExhausted { next: usize::MAX }),
    );
    assert_eq!(result, Err(ResError::NoMemory));
}

#[def_test]
fn native_irq_preserves_owner_preparation_and_hwirq_bounds_errors() {
    let route = IrqRouteDesc {
        controller: IrqController::IoApic,
        domain: Some(kirq::IO_APIC_DOMAIN),
        hwirq: 37,
        trigger: IrqTrigger::LevelHigh,
    };
    let resource = IrqResource::mapped(
        Virq::new(903),
        route.trigger,
        route.controller,
        route.domain,
        route.hwirq,
    );
    for (error, expected) in [
        (kirq::IrqDescError::NoMemory, ResError::NoMemory),
        (
            kirq::IrqDescError::InvalidHwirq { hwirq: 37 },
            ResError::InvalidResource,
        ),
    ] {
        assert_eq!(map_route_with(route, |_| Err(error)), Err(expected));
        assert_eq!(
            request_with(
                resource,
                Arc::new(|| IrqAction::Handled),
                true,
                |_| None,
                |_, _| Err(error),
            ),
            Err(expected)
        );
    }
}

#[def_test]
fn native_irq_configuration_errors_preserve_typed_request_failures() {
    let resource = IrqResource::mapped(
        Virq::new(903),
        IrqTrigger::LevelHigh,
        IrqController::Gic,
        Some(IrqDomainId::new(0x7051)),
        37,
    );
    for (error, expected) in [
        (
            kirq::IrqDescError::ConfigurationFailed { hwirq: 37 },
            ResError::Io,
        ),
        (
            kirq::IrqDescError::UnsupportedFlow { hwirq: 37 },
            ResError::Unsupported,
        ),
    ] {
        let handler: IrqHandlerFn = Arc::new(|| IrqAction::Handled);
        let mut registered = None;
        let result = request_with(
            resource,
            Arc::clone(&handler),
            true,
            |_| None,
            |spec, _| {
                registered = Some(spec);
                Err(error)
            },
        );
        assert_eq!(result, Err(expected));
        assert_eq!(registered, Some(resource_spec(resource)));
        assert_eq!(Arc::strong_count(&handler), 1);
    }
}

#[def_test]
fn native_irq_plain_request_validates_existing_descriptor_without_remapping() {
    let configured = kirq::IrqDesc::new(37, IrqTrigger::LevelHigh)
        .with_virq(903)
        .with_controller(IrqController::Gic)
        .with_domain(IrqDomainId::new(0x7051));
    for (descriptor, trigger, expected) in [
        (None, IrqTrigger::LevelHigh, Err(ResError::InvalidResource)),
        (
            Some(configured),
            IrqTrigger::EdgeRising,
            Err(ResError::InvalidResource),
        ),
        (
            Some(configured),
            IrqTrigger::LevelHigh,
            Ok(IrqHandlerToken::SharedAction(73)),
        ),
        (
            Some(configured),
            IrqTrigger::Unknown(0),
            Ok(IrqHandlerToken::SharedAction(73)),
        ),
    ] {
        let mut queried = None;
        let mut registered = None;
        let result = request_with(
            IrqResource::plain(Virq::new(903), trigger),
            Arc::new(|| IrqAction::Handled),
            true,
            |virq| {
                queried = Some(virq);
                descriptor
            },
            |spec, _| {
                registered = Some(spec);
                Ok(Some(kirq::IrqActionToken::new(73)))
            },
        );
        assert_eq!(result, expected);
        assert_eq!(queried, Some(903));
        assert_eq!(
            registered,
            expected.ok().map(|_| kirq::IrqSpec::PlainVirq(903))
        );
    }
}
