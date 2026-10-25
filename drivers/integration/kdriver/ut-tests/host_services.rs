// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec, vec::Vec};

use driver_model::{
    Bus, DeviceAccess, DeviceDescriptor, DeviceDomain, DeviceState, DriverDesc, DriverOps,
    Environment, IrqAction, IrqController, IrqDomainId, IrqTrigger, OfMatch, PlatformBus,
    PlatformBusData, PlatformId, ProbeError,
};
use driver_os_api::{IrqHandlerToken, ResError, ResResult};
use kspin::SpinNoIrq;
use unittest::{assert_eq, def_test};

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Map(IrqController, Option<IrqDomainId>, usize, IrqTrigger),
    Request(IrqResource, IrqAction),
    Release(IrqResource, IrqHandlerToken),
    Enable(IrqResource, bool),
}

struct TestIrq {
    events: Arc<SpinNoIrq<Vec<Event>>>,
    map_error: Option<ResError>,
    request_error: Option<ResError>,
}

impl IrqOp for TestIrq {
    fn map_irq(&self, route: IrqRouteDesc) -> ResResult<IrqResource> {
        self.events.lock().push(Event::Map(
            route.controller,
            route.domain,
            route.hwirq,
            route.trigger,
        ));
        if let Some(error) = self.map_error {
            return Err(error);
        }
        Ok(mapped_resource())
    }

    fn request_irq(
        &self,
        resource: IrqResource,
        handler: IrqHandlerFn,
    ) -> ResResult<IrqHandlerToken> {
        let action = handler();
        self.events.lock().push(Event::Request(resource, action));
        if let Some(error) = self.request_error {
            return Err(error);
        }
        Ok(IrqHandlerToken::SharedAction(91))
    }

    fn release_irq(&self, resource: IrqResource, token: IrqHandlerToken) {
        self.events.lock().push(Event::Release(resource, token));
    }

    fn set_irq_enabled(&self, resource: IrqResource, enabled: bool) {
        self.events.lock().push(Event::Enable(resource, enabled));
    }
}

fn mapped_resource() -> IrqResource {
    IrqResource::mapped(
        Virq::new(307),
        IrqTrigger::LevelHigh,
        IrqController::Gic,
        None,
        71,
    )
}

struct IrqProbe {
    spec: IrqSpec,
    fail_after_request: bool,
    result: Arc<SpinNoIrq<Option<Result<Virq, RuntimeError>>>>,
}

impl DriverOps<PlatformBus> for IrqProbe {
    type Data = DevIrq;

    fn probe(
        &self,
        device: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
    ) -> Result<DevIrq, ProbeError> {
        let result = device
            .res()
            .request_irq(self.spec, "host-services-ut", || IrqAction::NotHandled);
        *self.result.lock() = Some(match &result {
            Ok(guard) => Ok(guard.virq()),
            Err(ProbeError::Fatal(error)) => Err(*error),
            Err(_) => Err(RuntimeError::BadState),
        });
        let guard = result?;
        if self.fail_after_request {
            return Err(RuntimeError::Io.into());
        }
        Ok(guard)
    }
}

fn domain_with_driver(
    provider: Arc<dyn IrqOp>,
    driver: IrqProbe,
) -> (DeviceDomain, driver_model::DeviceRef) {
    let domain = DeviceDomain::new();
    domain.set_services(new_device_services(provider));
    let bus = domain
        .add_bus_instance(PlatformBus::ID, None, "host-services-ut")
        .unwrap();
    static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("test,host-services"))];
    domain.register_driver(DriverDesc::new("host-services-ut", IDS, driver));
    let device = domain
        .add_device(
            DeviceDescriptor::builder()
                .bus_instance(bus)
                .bus_data(Box::new(PlatformBusData::firmware_node(
                    1,
                    &["test,host-services"],
                )))
                .build(),
        )
        .unwrap();
    (domain, device)
}

#[def_test]
fn host_services_preserve_irq_mapping_and_release_each_binding_once() {
    for spec in [
        IrqSpec::firmware(IrqController::Gic, 7, IrqTrigger::EdgeRising),
        IrqSpec::resolved(Virq::new(19), IrqTrigger::LevelHigh),
    ] {
        let events = Arc::new(SpinNoIrq::new(Vec::new()));
        let provider = Arc::new(TestIrq {
            events: events.clone(),
            map_error: None,
            request_error: None,
        });
        let result = Arc::new(SpinNoIrq::new(None));
        let (domain, device) = domain_with_driver(
            provider,
            IrqProbe {
                spec,
                fail_after_request: false,
                result: result.clone(),
            },
        );
        let resource = match spec {
            IrqSpec::ResolvedVirq { virq, trigger } => IrqResource::plain(virq, trigger),
            _ => mapped_resource(),
        };
        let observed = *result.lock();
        assert_eq!(observed, Some(Ok(resource.virq())));
        assert_eq!(domain.snapshot(&device).unwrap().state, DeviceState::Active);
        domain.unbind_device(&device).unwrap();
        domain.bind_device(&device).unwrap();
        domain.remove_device(&device).unwrap();
        let mut expected = Vec::new();
        for _ in 0..2 {
            if let IrqSpec::FirmwareRoute {
                controller,
                domain,
                hwirq,
                trigger,
            } = spec
            {
                expected.push(Event::Map(controller, domain, hwirq, trigger));
            }
            expected.push(Event::Request(resource, IrqAction::NotHandled));
            expected.push(Event::Release(resource, IrqHandlerToken::SharedAction(91)));
        }
        let actual = events.lock().clone();
        assert_eq!(actual, expected);
    }
}

#[def_test]
fn host_services_irq_errors_and_failed_probe_do_not_double_release() {
    for (map_error, request_error, fail_after_request, expected_result, requests, releases) in [
        (
            Some(ResError::InvalidResource),
            None,
            false,
            Err(RuntimeError::Invalid),
            0,
            0,
        ),
        (
            None,
            Some(ResError::Busy),
            false,
            Err(RuntimeError::Busy),
            1,
            0,
        ),
        (None, None, true, Ok(Virq::new(307)), 1, 1),
    ] {
        let events = Arc::new(SpinNoIrq::new(Vec::new()));
        let provider = Arc::new(TestIrq {
            events: events.clone(),
            map_error,
            request_error,
        });
        let result = Arc::new(SpinNoIrq::new(None));
        let (domain, device) = domain_with_driver(
            provider,
            IrqProbe {
                spec: IrqSpec::firmware(IrqController::Gic, 7, IrqTrigger::EdgeRising),
                fail_after_request,
                result: result.clone(),
            },
        );
        assert_eq!(
            domain.snapshot(&device).unwrap().state,
            DeviceState::Unclaimed
        );
        domain.remove_device(&device).unwrap();
        let observed = *result.lock();
        assert_eq!(observed, Some(expected_result));
        let actual = events.lock().clone();
        assert_eq!(
            actual
                .iter()
                .filter(|event| matches!(event, Event::Request(..)))
                .count(),
            requests
        );
        assert_eq!(
            actual
                .iter()
                .filter(|event| matches!(event, Event::Release(..)))
                .count(),
            releases
        );
    }
}

#[def_test]
fn host_services_extensions_are_not_shared_between_domains() {
    struct Marker;
    impl driver_model::ExtensionService for Marker {}
    static MARKER: Marker = Marker;
    let provider = Arc::new(TestIrq {
        events: Arc::new(SpinNoIrq::new(vec![])),
        map_error: None,
        request_error: None,
    });
    let first = new_device_services(provider.clone());
    let second = new_device_services(provider);
    first.extensions().register(&MARKER);
    assert_eq!(first.extensions().get::<Marker>().is_some(), true);
    assert_eq!(second.extensions().get::<Marker>().is_none(), true);
}
