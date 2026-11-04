// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn explicit_domain_is_preserved_for_route_and_resource() {
    let domain = driver_os_api::IrqDomainId::new(0x7319);
    let route = IrqRouteDesc {
        hwirq: 37,
        trigger: DevIrqTrigger::Unknown(0x81),
        controller: DevIrqController::Gic,
        domain: Some(domain),
    };
    let expected = kirq::IrqDesc::new(37, route.trigger)
        .with_controller(route.controller)
        .with_domain(domain);
    assert_eq!(irq_route_to_kirq_desc(route), expected);
    let resource = IrqResource::new(903, route.trigger)
        .with_controller(route.controller)
        .with_domain(domain)
        .with_hwirq(route.hwirq);
    assert_eq!(
        irq_resource_to_kirq_spec(resource),
        kirq::IrqSpec::Desc(expected.with_virq(903))
    );
}

#[def_test]
fn mapped_resource_retains_domain_and_nonidentity_virq() {
    let explicit = driver_os_api::IrqDomainId::new(0x7319);
    for (controller, supplied, expected) in [
        (DevIrqController::Gic, Some(explicit), Some(explicit)),
        (DevIrqController::Unknown, Some(explicit), Some(explicit)),
        (DevIrqController::Gic, None, Some(kirq::GIC_ROOT_DOMAIN)),
        (DevIrqController::Plic, None, Some(kirq::PLIC_ROOT_DOMAIN)),
        (DevIrqController::IoApic, None, Some(kirq::IO_APIC_DOMAIN)),
        (DevIrqController::Unknown, None, None),
        (DevIrqController::LoongArchExtioi, None, None),
        (DevIrqController::Msi, None, None),
    ] {
        let route = IrqRouteDesc {
            hwirq: 37,
            trigger: DevIrqTrigger::Unknown(0x81),
            controller,
            domain: supplied,
        };
        let mut observed = None;
        let resource = map_route_with(route, |desc| {
            observed = Some(desc);
            Ok(903)
        })
        .unwrap();
        let desc = observed.unwrap();
        assert_eq!(desc.domain, expected);
        assert_eq!(desc.controller, controller);
        assert_eq!(desc.hwirq, 37);
        assert_eq!(resource.number, 903);
        assert_eq!(resource.domain, expected);
        assert_eq!(resource.trigger, route.trigger);
        assert_eq!(
            irq_resource_to_kirq_spec(resource),
            kirq::IrqSpec::Desc(desc.with_virq(903))
        );
    }
}

#[def_test]
fn rejected_explicit_domain_never_retries_with_a_root_domain() {
    let domain = driver_os_api::IrqDomainId::new(0x7319);
    let route = IrqRouteDesc {
        hwirq: 37,
        trigger: DevIrqTrigger::LevelHigh,
        controller: DevIrqController::Gic,
        domain: Some(domain),
    };
    let mut observed = None;
    let result = map_route_with(route, |desc| {
        observed = Some(desc.domain);
        Err(kirq::IrqDescError::UnknownDomain { domain })
    });
    assert_eq!(observed, Some(Some(domain)));
    assert_eq!(result, Err(ResError::InvalidResource));
}

#[def_test]
fn test_plain_irq_resource_maps_to_plain_virq_spec() {
    let resource = IrqResource::new(32, DevIrqTrigger::Unknown(0));

    let spec = irq_resource_to_kirq_spec(resource);

    assert_eq!(spec, kirq::IrqSpec::PlainVirq(32));
}

#[def_test]
fn test_routed_irq_resource_maps_to_descriptor_spec() {
    let resource = IrqResource::new(48, DevIrqTrigger::LevelHigh)
        .with_controller(DevIrqController::Gic)
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
    let event = dev_irq_event_to_kirq(DevIrqEvent::wake_thread_from_sources(0b1010));

    assert!(event.handled());
    assert_eq!(event.sources(), 0b1010);
}

#[def_test]
fn test_device_irq_event_handled_does_not_wake_thread() {
    let event = dev_irq_event_to_kirq(DevIrqEvent::from_sources(0b0101));

    assert!(event.handled());
    assert_eq!(event.sources(), 0b0101);
}
