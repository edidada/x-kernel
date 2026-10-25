// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sequential lifecycle and nested dispatch contracts; host contention belongs
//! to kdriver tests.

use unittest::{assert, assert_eq, def_test};

use super::*;

type Events = Arc<SpinNoPreempt<Vec<&'static str>>>;

/// Synthetic commit-validation failure, not a reachable management race:
/// the probe deliberately edits the generation while owning management.
#[def_test]
fn uncommitted_probe_data_is_removed_before_drop_and_can_bind_again() {
    struct CommitFailureDriver {
        domain: alloc::sync::Weak<DeviceDomain>,
        should_invalidate: Arc<AtomicBool>,
        events: Events,
    }
    impl DriverOps<PlatformBus> for CommitFailureDriver {
        type Data = LifecycleData;

        fn probe(
            &self,
            dev: &mut DeviceAccess<'_, PlatformBus>,
            _env: &crate::Environment<'_>,
        ) -> Result<Self::Data, ProbeError> {
            self.events.lock().push("probe");
            if self.should_invalidate.swap(false, Ordering::Relaxed) {
                let domain = self.domain.upgrade().unwrap();
                let mut inner = domain.inner.lock();
                let entry = inner
                    .devices
                    .get_mut(&dev.reference().core.id.raw())
                    .unwrap();
                core::assert_eq!(entry.lifecycle.state, DeviceState::Probing);
                entry.lifecycle.generation += 1;
            }
            Ok(LifecycleData {
                events: self.events.clone(),
            })
        }

        fn remove(
            &self,
            _dev: &mut DeviceAccess<'_, PlatformBus>,
            _env: &crate::Environment<'_>,
            _data: &mut Self::Data,
        ) {
            self.events.lock().push("remove");
        }
    }

    let (domain, _services, root) = fresh_domain();
    let domain = Arc::new(domain);
    let device = domain.add_device(test_descriptor(root)).unwrap();
    let events = Arc::new(SpinNoPreempt::new(Vec::new()));
    let should_invalidate = Arc::new(AtomicBool::new(true));
    device
        .core
        .management
        .run_locked(&mut || {
            core::assert!(domain.register_driver(DriverDesc::new(
                "commit-failure",
                IDS,
                CommitFailureDriver {
                    domain: Arc::downgrade(&domain),
                    should_invalidate: should_invalidate.clone(),
                    events: events.clone(),
                },
            )));
        })
        .unwrap();
    assert_eq!(
        domain.snapshot(&device).unwrap().state,
        DeviceState::Pending
    );
    assert!(events.lock().is_empty());
    assert_eq!(
        domain.probe_pending(),
        ProbeStats {
            failed: 1,
            ..ProbeStats::default()
        }
    );
    assert_eq!(*events.lock(), alloc::vec!["probe", "remove", "drop"]);
    assert_eq!(
        domain.snapshot(&device).unwrap().state,
        DeviceState::Unclaimed
    );
    assert_eq!(device.driver_name(), None);
    assert!(
        domain
            .with_data::<LifecycleData, ()>(&device, |_| ())
            .is_none()
    );
    assert_eq!(domain.devices().len(), 1);
    assert_eq!(domain.probe_pending(), ProbeStats::default());

    events.lock().clear();
    should_invalidate.store(true, Ordering::Relaxed);
    assert_eq!(domain.bind_device(&device), Err(RuntimeError::BadState));
    assert_eq!(*events.lock(), alloc::vec!["probe", "remove", "drop"]);
    assert_eq!(
        domain.snapshot(&device).unwrap().state,
        DeviceState::Unclaimed
    );
    assert_eq!(device.driver_name(), None);
    assert!(
        domain
            .with_data::<LifecycleData, ()>(&device, |_| ())
            .is_none()
    );

    events.lock().clear();
    assert_eq!(domain.bind_device(&device).unwrap().activated, 1);
    assert_eq!(*events.lock(), alloc::vec!["probe"]);
    assert_eq!(domain.snapshot(&device).unwrap().state, DeviceState::Active);
    domain.remove_device(&device).unwrap();
    assert_eq!(*events.lock(), alloc::vec!["probe", "remove", "drop"]);
    assert!(domain.devices().is_empty());
}

#[def_test]
fn service_replacement_preserves_each_devices_registration_host() {
    struct HostIdentity(&'static str);
    impl crate::ExtensionService for HostIdentity {}
    static HOST_A: HostIdentity = HostIdentity("A");
    static HOST_B: HostIdentity = HostIdentity("B");

    struct HostDriver {
        events: Arc<SpinNoPreempt<Vec<(&'static str, &'static str)>>>,
    }
    impl DriverOps<PlatformBus> for HostDriver {
        type Data = IoMap;

        fn probe(
            &self,
            dev: &mut DeviceAccess<'_, PlatformBus>,
            env: &crate::Environment<'_>,
        ) -> Result<Self::Data, ProbeError> {
            let host = env.extension::<HostIdentity>().unwrap();
            self.events.lock().push(("probe", host.0));
            dev.res().iomap(0)
        }

        fn remove(
            &self,
            dev: &mut DeviceAccess<'_, PlatformBus>,
            env: &crate::Environment<'_>,
            _data: &mut Self::Data,
        ) {
            let host = env.extension::<HostIdentity>().unwrap();
            self.events.lock().push(("remove", host.0));
            drop(dev.res().iomap(0).unwrap());
        }
    }

    let (domain, services_a, root) = fresh_domain();
    services_a.extensions.register(&HOST_A);
    let services_b = MockServices::new();
    services_b.extensions.register(&HOST_B);
    let events = Arc::new(SpinNoPreempt::new(Vec::new()));
    domain.register_driver(DriverDesc::new(
        "host-identity",
        IDS,
        HostDriver {
            events: events.clone(),
        },
    ));
    let device_a = domain.add_device(test_descriptor(root)).unwrap();
    domain.set_services(services_b.clone());
    domain.unbind_device(&device_a).unwrap();
    assert_eq!(services_a.mapped.load(Ordering::Relaxed), 2);
    assert_eq!(services_a.released.load(Ordering::Relaxed), 2);
    assert_eq!(services_b.mapped.load(Ordering::Relaxed), 0);
    assert_eq!(domain.bind_device(&device_a).unwrap().activated, 1);
    let device_b = domain
        .add_device(test_descriptor_for_node(root, 2))
        .unwrap();
    assert_eq!(
        domain.snapshot(&device_a).unwrap().state,
        DeviceState::Active
    );
    assert_eq!(
        domain.snapshot(&device_b).unwrap().state,
        DeviceState::Active
    );
    domain.remove_device(&device_a).unwrap();
    domain.remove_device(&device_b).unwrap();
    assert_eq!(
        *events.lock(),
        alloc::vec![
            ("probe", "A"),
            ("remove", "A"),
            ("probe", "A"),
            ("probe", "B"),
            ("remove", "A"),
            ("remove", "B")
        ]
    );
    assert_eq!(services_a.mapped.load(Ordering::Relaxed), 4);
    assert_eq!(services_a.released.load(Ordering::Relaxed), 4);
    assert_eq!(services_b.mapped.load(Ordering::Relaxed), 2);
    assert_eq!(services_b.released.load(Ordering::Relaxed), 2);
    for (services, device, count) in [(&services_a, &device_a, 4), (&services_b, &device_b, 2)] {
        let requests = services.requests.lock();
        assert_eq!(requests.len(), count);
        assert!(requests.iter().all(|request| {
            request.operation == "iomap"
                && request
                    .device
                    .upgrade()
                    .is_some_and(|core| Arc::ptr_eq(&core, &device.core))
        }));
    }
    assert!(domain.devices().is_empty());
}

#[def_test(serial)]
fn rejected_probe_stays_pending_and_retries_after_management_exit() {
    let (domain, services, root) = fresh_domain();
    domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));
    let device = domain.add_device(test_descriptor(root)).unwrap();
    assert_eq!(
        domain.snapshot(&device).unwrap().state,
        DeviceState::Deferred
    );
    services.publish_clk(0x42);
    let probes_before = PROBES.load(Ordering::Relaxed);
    let mut rejected = ProbeStats::default();
    device
        .core
        .management
        .run_locked(&mut || {
            core::assert_eq!(domain.wake(DepSource::clk(0x42)), 1);
            core::assert_eq!(
                domain.snapshot(&device).unwrap().state,
                DeviceState::Pending
            );
            rejected = domain.probe_pending();
            core::assert_eq!(PROBES.load(Ordering::Relaxed), probes_before);
            core::assert_eq!(
                domain.snapshot(&device).unwrap().state,
                DeviceState::Pending
            );
        })
        .unwrap();
    assert_eq!(
        rejected,
        ProbeStats {
            rejected: 1,
            ..ProbeStats::default()
        }
    );
    assert_eq!(domain.devices().len(), 1);
    assert!(domain.deferred_report().is_empty());
    assert_eq!(
        domain.probe_pending(),
        ProbeStats {
            activated: 1,
            ..ProbeStats::default()
        }
    );
    assert_eq!(PROBES.load(Ordering::Relaxed), probes_before + 1);
    assert_eq!(domain.snapshot(&device).unwrap().state, DeviceState::Active);
    assert_eq!(domain.probe_pending(), ProbeStats::default());
    domain.remove_device(&device).unwrap();
}

#[def_test]
fn nested_add_binds_another_device_before_outer_probe_returns() {
    struct EnumeratingDriver {
        domain: alloc::sync::Weak<DeviceDomain>,
        root: BusInstanceId,
        events: Events,
    }
    impl DriverOps<PlatformBus> for EnumeratingDriver {
        type Data = crate::DeviceRef;

        fn probe(
            &self,
            dev: &mut DeviceAccess<'_, PlatformBus>,
            _env: &crate::Environment<'_>,
        ) -> Result<Self::Data, ProbeError> {
            let domain = self.domain.upgrade().unwrap();
            self.events.lock().push("outer-enter");
            let mut descriptor = test_descriptor_for_node(self.root, 2);
            descriptor.parent = Some(dev.reference());
            let child = domain.add_device(descriptor).unwrap();
            core::assert_eq!(domain.snapshot(&child).unwrap().state, DeviceState::Active);
            core::assert_eq!(
                domain.snapshot(&dev.reference()).unwrap().state,
                DeviceState::Probing
            );
            self.events.lock().push("outer-exit");
            Ok(child)
        }

        fn remove(
            &self,
            _dev: &mut DeviceAccess<'_, PlatformBus>,
            _env: &crate::Environment<'_>,
            child: &mut Self::Data,
        ) {
            self.domain.upgrade().unwrap().remove_device(child).unwrap();
            self.events.lock().push("outer-remove");
        }
    }
    let (domain, _services, root) = fresh_domain();
    let domain = Arc::new(domain);
    let events = Arc::new(SpinNoPreempt::new(Vec::new()));
    domain.register_driver(DriverDesc::new(
        "nested-child",
        IDS,
        LifecycleDriver {
            events: events.clone(),
        },
    ));
    static OUTER_IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("nested,outer"))];
    domain.register_driver(DriverDesc::new(
        "enumerator",
        OUTER_IDS,
        EnumeratingDriver {
            domain: Arc::downgrade(&domain),
            root,
            events: events.clone(),
        },
    ));
    let outer = domain
        .add_device(
            DeviceDescriptor::builder()
                .bus_instance(root)
                .bus_data(Box::new(PlatformBusData::firmware_node(
                    1,
                    &["nested,outer"],
                )))
                .build(),
        )
        .unwrap();
    assert_eq!(
        *events.lock(),
        alloc::vec!["outer-enter", "probe", "outer-exit"]
    );
    assert_eq!(domain.snapshot(&outer).unwrap().state, DeviceState::Active);
    assert_eq!(domain.devices().len(), 2);
    domain.remove_device(&outer).unwrap();
    assert_eq!(
        *events.lock(),
        alloc::vec![
            "outer-enter",
            "probe",
            "outer-exit",
            "remove",
            "drop",
            "outer-remove"
        ]
    );
    assert!(domain.devices().is_empty());
}

struct LifecycleData {
    events: Events,
}

impl Drop for LifecycleData {
    fn drop(&mut self) {
        self.events.lock().push("drop");
    }
}

struct LifecycleDriver {
    events: Events,
}

impl DriverOps<PlatformBus> for LifecycleDriver {
    type Data = LifecycleData;

    fn probe(
        &self,
        _dev: &mut DeviceAccess<'_, PlatformBus>,
        _env: &crate::Environment<'_>,
    ) -> Result<Self::Data, ProbeError> {
        self.events.lock().push("probe");
        Ok(LifecycleData {
            events: self.events.clone(),
        })
    }

    fn remove(
        &self,
        _dev: &mut DeviceAccess<'_, PlatformBus>,
        _env: &crate::Environment<'_>,
        _data: &mut Self::Data,
    ) {
        self.events.lock().push("remove");
    }
}

#[def_test]
fn bind_unbind_rebind_unregister_orders_remove_before_each_data_drop() {
    let (domain, _services, root) = fresh_domain();
    let device = domain.add_device(test_descriptor(root)).unwrap();
    assert_eq!(
        domain.snapshot(&device).unwrap().state,
        DeviceState::Unclaimed
    );
    assert_eq!(domain.bind_device(&device).unwrap().activated, 0);
    let events = Arc::new(SpinNoPreempt::new(Vec::new()));
    domain.register_driver(DriverDesc::new(
        "lifecycle",
        IDS,
        LifecycleDriver {
            events: events.clone(),
        },
    ));
    assert_eq!(*events.lock(), alloc::vec!["probe"]);
    assert_eq!(domain.snapshot(&device).unwrap().state, DeviceState::Active);
    assert_eq!(domain.bind_device(&device).unwrap(), ProbeStats::default());
    assert_eq!(*events.lock(), alloc::vec!["probe"]);
    let retained = device.clone();
    let weak = Arc::downgrade(&device.core);
    domain.unbind_device(&device).unwrap();
    assert_eq!(*events.lock(), alloc::vec!["probe", "remove", "drop"]);
    assert_eq!(
        domain.snapshot(&device).unwrap().state,
        DeviceState::Unclaimed
    );
    assert_eq!(device.driver_name(), None);
    assert_eq!(domain.devices().len(), 1);
    assert!(domain.devices()[0].device.same_device(&retained));
    assert!(
        domain
            .with_data::<LifecycleData, ()>(&device, |_| ())
            .is_none()
    );
    assert_eq!(domain.bus_instance_devices(root).len(), 1);
    domain.unbind_device(&device).unwrap();
    assert_eq!(*events.lock(), alloc::vec!["probe", "remove", "drop"]);
    assert_eq!(domain.bind_device(&device).unwrap().activated, 1);
    assert_eq!(domain.snapshot(&device).unwrap().state, DeviceState::Active);
    assert_eq!(device.driver_name(), Some("lifecycle"));
    assert!(retained.is_online());
    assert!(
        domain
            .snapshot(&device)
            .unwrap()
            .device
            .same_device(&retained)
    );
    domain.remove_device(&device).unwrap();
    assert_eq!(
        *events.lock(),
        alloc::vec!["probe", "remove", "drop", "probe", "remove", "drop"]
    );
    assert!(domain.snapshot(&device).is_none());
    assert!(domain.devices().is_empty());
    assert!(domain.bus_instance_devices(root).is_empty());
    assert!(!retained.is_online());
    assert_eq!(retained.path(), "platform/firmware1");
    domain.remove_device(&device).unwrap();
    assert_eq!(domain.bind_device(&device), Err(RuntimeError::NotFound));
    drop(device);
    assert!(weak.upgrade().is_some());
    drop(retained);
    assert!(weak.upgrade().is_none());
    assert_eq!(
        *events.lock(),
        alloc::vec!["probe", "remove", "drop", "probe", "remove", "drop"]
    );
}

#[def_test]
fn unregister_after_unbind_does_not_remove_or_drop_the_binding_twice() {
    let (domain, _services, root) = fresh_domain();
    let events = Arc::new(SpinNoPreempt::new(Vec::new()));
    domain.register_driver(DriverDesc::new(
        "lifecycle",
        IDS,
        LifecycleDriver {
            events: events.clone(),
        },
    ));
    let device = domain.add_device(test_descriptor(root)).unwrap();
    domain.unbind_device(&device).unwrap();
    domain.remove_device(&device).unwrap();
    domain.remove_device(&device).unwrap();
    assert_eq!(*events.lock(), alloc::vec!["probe", "remove", "drop"]);
    assert!(domain.snapshot(&device).is_none());
    assert!(domain.devices().is_empty());
}

#[def_test]
fn failed_or_deferred_probe_never_calls_remove_without_bound_data() {
    struct UnboundDriver {
        removes: Arc<AtomicUsize>,
        failure: ProbeError,
    }
    impl DriverOps<PlatformBus> for UnboundDriver {
        type Data = ();

        fn probe(
            &self,
            _dev: &mut DeviceAccess<'_, PlatformBus>,
            _env: &crate::Environment<'_>,
        ) -> Result<(), ProbeError> {
            Err(self.failure.clone())
        }

        fn remove(
            &self,
            _dev: &mut DeviceAccess<'_, PlatformBus>,
            _env: &crate::Environment<'_>,
            _data: &mut (),
        ) {
            self.removes.fetch_add(1, Ordering::Relaxed);
        }
    }
    for failure in [
        ProbeError::Fatal(RuntimeError::Io),
        ProbeError::Deferred(DepSource::clk(0x42)),
    ] {
        let (domain, _services, root) = fresh_domain();
        let removes = Arc::new(AtomicUsize::new(0));
        domain.register_driver(DriverDesc::new(
            "unbound",
            IDS,
            UnboundDriver {
                removes: removes.clone(),
                failure: failure.clone(),
            },
        ));
        let device = domain.add_device(test_descriptor(root)).unwrap();
        let expected = match failure {
            ProbeError::Fatal(_) => DeviceState::Unclaimed,
            ProbeError::Deferred(_) => DeviceState::Deferred,
        };
        assert_eq!(domain.snapshot(&device).unwrap().state, expected);
        domain.unbind_device(&device).unwrap();
        assert_eq!(
            domain.snapshot(&device).unwrap().state,
            DeviceState::Unclaimed
        );
        domain.remove_device(&device).unwrap();
        domain.remove_device(&device).unwrap();
        assert_eq!(removes.load(Ordering::Relaxed), 0);
        assert!(domain.devices().is_empty());
        assert!(domain.deferred_report().is_empty());
        domain.wake(DepSource::clk(0x42));
        assert_eq!(domain.probe_pending().activated, 0);
    }
}

#[def_test]
fn missing_management_services_reject_registration_without_residue() {
    let domain = DeviceDomain::new();
    let root = domain
        .add_bus_instance(PlatformBus::ID, None, "no-management")
        .unwrap();
    assert!(matches!(
        domain.add_device(test_descriptor(root)),
        Err(AddDeviceError::Management(RuntimeError::Unsupported))
    ));
    assert!(domain.devices().is_empty());
    assert!(domain.bus_instance_devices(root).is_empty());
    assert!(domain.deferred_report().is_empty());
    assert_eq!(domain.probe_pending(), ProbeStats::default());
    domain.set_services(Arc::new(NoServices));
    assert!(matches!(
        domain.add_device(test_descriptor(root)),
        Err(AddDeviceError::Management(RuntimeError::Unsupported))
    ));
    assert!(domain.devices().is_empty());
    assert!(domain.bus_instance_devices(root).is_empty());
    assert_eq!(domain.probe_pending(), ProbeStats::default());
    domain.set_services(MockServices::new());
    let device = domain.add_device(test_descriptor(root)).unwrap();
    assert_eq!(domain.devices().len(), 1);
    assert_eq!(domain.bus_instance_devices(root).len(), 1);
    assert_eq!(
        domain.snapshot(&device).unwrap().state,
        DeviceState::Unclaimed
    );
    domain.remove_device(&device).unwrap();
}

#[def_test]
fn same_device_callback_reentry_is_rejected_without_disturbing_outer_lifecycle() {
    struct ReentrantDriver {
        domain: alloc::sync::Weak<DeviceDomain>,
        removes: Arc<AtomicUsize>,
    }
    impl ReentrantDriver {
        fn assert_rejected(&self, device: &crate::DeviceRef) {
            let domain = self.domain.upgrade().unwrap();
            let expected = RuntimeError::Management(crate::DeviceManagementError::Reentrant);
            core::assert_eq!(domain.remove_device(device), Err(expected));
            core::assert_eq!(domain.unbind_device(device), Err(expected));
            core::assert_eq!(domain.bind_device(device), Err(expected));
        }
    }
    impl DriverOps<PlatformBus> for ReentrantDriver {
        type Data = ();

        fn probe(
            &self,
            dev: &mut DeviceAccess<'_, PlatformBus>,
            _env: &crate::Environment<'_>,
        ) -> Result<(), ProbeError> {
            self.assert_rejected(&dev.reference());
            Ok(())
        }

        fn remove(
            &self,
            dev: &mut DeviceAccess<'_, PlatformBus>,
            _env: &crate::Environment<'_>,
            _data: &mut (),
        ) {
            self.assert_rejected(&dev.reference());
            self.removes.fetch_add(1, Ordering::Relaxed);
        }
    }
    let (domain, _services, root) = fresh_domain();
    let domain = Arc::new(domain);
    let removes = Arc::new(AtomicUsize::new(0));
    domain.register_driver(DriverDesc::new(
        "reentrant",
        IDS,
        ReentrantDriver {
            domain: Arc::downgrade(&domain),
            removes: removes.clone(),
        },
    ));
    let device = domain.add_device(test_descriptor(root)).unwrap();
    assert_eq!(domain.snapshot(&device).unwrap().state, DeviceState::Active);
    domain.unbind_device(&device).unwrap();
    assert_eq!(removes.load(Ordering::Relaxed), 1);
    assert_eq!(
        domain.snapshot(&device).unwrap().state,
        DeviceState::Unclaimed
    );
    assert_eq!(domain.bind_device(&device).unwrap().activated, 1);
    domain.remove_device(&device).unwrap();
    assert_eq!(removes.load(Ordering::Relaxed), 2);
    assert!(domain.snapshot(&device).is_none());
    assert!(domain.devices().is_empty());
}
