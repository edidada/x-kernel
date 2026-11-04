// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Binding ownership and rollback with real host management/drain providers.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use driver_model::{
    Bus, DeviceAccess, DeviceDescriptor, DeviceDomain, DeviceState, DriverDesc, DriverOps,
    Environment, OfMatch, PlatformBus, PlatformBusData, PlatformId, ProbeError, RuntimeError,
};
use kerrno::{KError, KResult};
use kspin::{IrqSave, SpinNoPreempt};
use subsys_hwrng::{HwrngDevice, HwrngHandle, HwrngRegistration, HwrngRegistry};
use unittest::{assert, assert_eq, def_test};

use super::device_management_lifecycle_unittest::HostServices;
use crate::rng_completion::new_rng_drain;

static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("test,binding-rng"))];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    None,
    Resource,
    BeforePublish,
    AfterPublish,
}

struct Observations {
    failure: Failure,
    can_probe: AtomicBool,
    events: SpinNoPreempt<Vec<&'static str>>,
    handles: SpinNoPreempt<Vec<HwrngHandle>>,
    removes: AtomicUsize,
    runtime_drops: AtomicUsize,
    ops_drops: AtomicUsize,
    cleanup_failed: AtomicBool,
}

impl Observations {
    fn new(failure: Failure) -> Arc<Self> {
        Arc::new(Self {
            failure,
            can_probe: AtomicBool::new(true),
            events: SpinNoPreempt::new(Vec::with_capacity(64)),
            handles: SpinNoPreempt::new(Vec::new()),
            removes: AtomicUsize::new(0),
            runtime_drops: AtomicUsize::new(0),
            ops_drops: AtomicUsize::new(0),
            cleanup_failed: AtomicBool::new(false),
        })
    }

    fn record(&self, event: &'static str) {
        self.events.lock().push(event);
    }
}

struct ResourceGuard(Arc<Observations>);

impl Drop for ResourceGuard {
    fn drop(&mut self) {
        self.0.record("release");
    }
}

struct Runtime {
    observations: Arc<Observations>,
    is_stopped: AtomicBool,
}

impl HwrngDevice for Runtime {
    fn read(&self, buf: &mut [u8]) -> KResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.is_stopped.load(Ordering::Acquire) {
            return Err(KError::BadState);
        }
        buf[0] = 0x39;
        Ok(1)
    }

    fn is_failed(&self) -> bool {
        self.is_stopped.load(Ordering::Acquire)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.observations
            .runtime_drops
            .fetch_add(1, Ordering::Relaxed);
    }
}

struct BindingData {
    registration: Option<HwrngRegistration>,
    runtime: Arc<Runtime>,
    resources: Option<ResourceGuard>,
}

impl BindingData {
    fn shutdown(&mut self) -> KResult {
        self.runtime.is_stopped.store(true, Ordering::Release);
        self.runtime.observations.record("stop");
        if let Some(registration) = &mut self.registration {
            registration.unregister()?;
            self.runtime.observations.record("unregister");
        }
        drop(self.resources.take());
        Ok(())
    }
}

impl Drop for BindingData {
    fn drop(&mut self) {
        self.runtime.observations.record("data-drop");
    }
}

struct PublishingDriver {
    registry: &'static HwrngRegistry,
    observations: Arc<Observations>,
}

impl Drop for PublishingDriver {
    fn drop(&mut self) {
        self.observations.ops_drops.fetch_add(1, Ordering::Relaxed);
    }
}

impl DriverOps<PlatformBus> for PublishingDriver {
    type Data = BindingData;

    fn probe(
        &self,
        dev: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
    ) -> Result<Self::Data, ProbeError> {
        if !self.observations.can_probe.load(Ordering::Relaxed) {
            return Err(RuntimeError::Unsupported.into());
        }
        let drain = new_rng_drain().map_err(|_| RuntimeError::BadState)?;
        if self.observations.failure == Failure::Resource {
            return Err(RuntimeError::NoMemory.into());
        }
        let resources = ResourceGuard(self.observations.clone());
        self.observations.record("acquire");
        let runtime = Arc::new(Runtime {
            observations: self.observations.clone(),
            is_stopped: AtomicBool::new(false),
        });
        self.observations.record("start");
        let mut data = BindingData {
            registration: None,
            runtime,
            resources: Some(resources),
        };
        if self.observations.failure == Failure::BeforePublish {
            data.shutdown().map_err(|_| RuntimeError::BadState)?;
            return Err(RuntimeError::Io.into());
        }
        let registration =
            match self
                .registry
                .register(dev.publication_source(), data.runtime.clone(), drain)
            {
                Ok(registration) => registration,
                Err(_) => {
                    data.shutdown().map_err(|_| RuntimeError::BadState)?;
                    return Err(RuntimeError::BadState.into());
                }
            };
        self.observations
            .handles
            .lock()
            .push(registration.publication());
        self.observations.record("publish");
        data.registration = Some(registration);
        if self.observations.failure == Failure::AfterPublish {
            data.shutdown().map_err(|_| RuntimeError::BadState)?;
            return Err(RuntimeError::Io.into());
        }
        Ok(data)
    }

    fn remove(
        &self,
        _: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
        data: &mut Self::Data,
    ) {
        self.observations.removes.fetch_add(1, Ordering::Relaxed);
        self.observations
            .cleanup_failed
            .store(data.shutdown().is_err(), Ordering::Relaxed);
    }
}

fn domain() -> (DeviceDomain, driver_model::BusInstanceId) {
    let domain = DeviceDomain::new();
    domain.set_services(Arc::new(HostServices::new()));
    let root = domain
        .add_bus_instance(PlatformBus::ID, None, "binding-ut")
        .unwrap();
    (domain, root)
}

fn descriptor(root: driver_model::BusInstanceId, node: u32) -> DeviceDescriptor {
    DeviceDescriptor::builder()
        .bus_instance(root)
        .bus_data(Box::new(PlatformBusData::firmware_node(
            node,
            &["test,binding-rng"],
        )))
        .build()
}

#[def_test(serial)]
fn probe_failure_rolls_back_before_return_without_framework_remove() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    for failure in [
        Failure::Resource,
        Failure::BeforePublish,
        Failure::AfterPublish,
    ] {
        let (domain, root) = domain();
        let observations = Observations::new(failure);
        domain.register_driver(DriverDesc::new(
            "failure-ut",
            IDS,
            PublishingDriver {
                registry: &REGISTRY,
                observations: observations.clone(),
            },
        ));
        let device = domain.add_device(descriptor(root, 1)).unwrap();
        let state = domain.snapshot(&device).unwrap().state;
        domain.remove_device(&device).unwrap();
        drop(domain);
        let handles = core::mem::take(&mut *observations.handles.lock());
        assert_eq!(state, DeviceState::Unclaimed);
        assert!(REGISTRY.devices().is_empty());
        assert_eq!(device.driver_name(), None);
        assert_eq!(observations.removes.load(Ordering::Relaxed), 0);
        assert_eq!(observations.ops_drops.load(Ordering::Relaxed), 1);
        let expected: &[&str] = match failure {
            Failure::Resource => &[],
            Failure::BeforePublish => &["acquire", "start", "stop", "release", "data-drop"],
            Failure::AfterPublish => &[
                "acquire",
                "start",
                "publish",
                "stop",
                "unregister",
                "release",
                "data-drop",
            ],
            Failure::None => unreachable!(),
        };
        let events = observations.events.lock().clone();
        assert_eq!(events.as_slice(), expected);
        for handle in &handles {
            assert_eq!(handle.driver_name(), Some("failure-ut"));
            assert_eq!(handle.read(&mut [0]), Err(KError::BadState));
        }
        drop(handles);
        assert_eq!(
            observations.runtime_drops.load(Ordering::Relaxed),
            usize::from(failure != Failure::Resource)
        );
        let events = observations.events.lock().clone();
        assert_eq!(events.as_slice(), expected);
    }
}

#[def_test(serial)]
fn rebind_preserves_provenance_and_does_not_retain_ops_or_repeat_cleanup() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let (domain, root) = domain();
    let first = Observations::new(Failure::None);
    domain.register_driver(DriverDesc::new(
        "driver-a",
        IDS,
        PublishingDriver {
            registry: &REGISTRY,
            observations: first.clone(),
        },
    ));
    let device = domain.add_device(descriptor(root, 1)).unwrap();
    let sibling = domain.add_device(descriptor(root, 2)).unwrap();
    let mut first_handles = core::mem::take(&mut *first.handles.lock());
    let old = first_handles.remove(0);
    let sibling_handle = first_handles.remove(0);
    let rejected = {
        let _guard = IrqSave::new();
        domain.unbind_device(&device)
    };
    let unchanged_on_rejection = old.is_available();
    domain.unbind_device(&device).unwrap();
    let first_remove_count = first.removes.load(Ordering::Relaxed);
    let old_read = old.read(&mut [0]);
    let sibling_read = sibling_handle.read(&mut [0]);
    first.can_probe.store(false, Ordering::Relaxed);
    let second = Observations::new(Failure::None);
    domain.register_driver(DriverDesc::new(
        "driver-b",
        IDS,
        PublishingDriver {
            registry: &REGISTRY,
            observations: second.clone(),
        },
    ));
    let replacement = second.handles.lock().pop().unwrap();
    let names_after_replacement = (
        device.driver_name(),
        old.driver_name(),
        replacement.driver_name(),
    );
    domain.unbind_device(&device).unwrap();
    let activated = domain.bind_device(&device).unwrap().activated;
    let rebound = second.handles.lock().pop().unwrap();
    let is_distinct = !replacement.same_publication(&rebound);
    let replacement_read = replacement.read(&mut [0]);
    let rebound_read = rebound.read(&mut [0]);
    domain.remove_device(&device).unwrap();
    domain.remove_device(&sibling).unwrap();
    drop(domain);
    let retained_names = (old.driver_name(), rebound.driver_name());
    let retained_ops_drops = (
        first.ops_drops.load(Ordering::Relaxed),
        second.ops_drops.load(Ordering::Relaxed),
    );
    let drops_before_release = (
        first.runtime_drops.load(Ordering::Relaxed),
        second.runtime_drops.load(Ordering::Relaxed),
    );
    let first_events = first.events.lock().clone();
    let second_events = second.events.lock().clone();
    drop((device, sibling));
    {
        let _guard = IrqSave::new();
        drop((old, sibling_handle, replacement, rebound));
    }
    assert!(matches!(rejected, Err(RuntimeError::Management(_))));
    assert!(unchanged_on_rejection);
    assert_eq!(first_remove_count, 1);
    assert_eq!(old_read, Err(KError::BadState));
    assert_eq!(sibling_read, Ok(1));
    assert_eq!(
        names_after_replacement,
        (Some("driver-b"), Some("driver-a"), Some("driver-b"))
    );
    assert_eq!(activated, 1);
    assert!(is_distinct);
    assert_eq!(replacement_read, Err(KError::BadState));
    assert_eq!(rebound_read, Ok(1));
    assert!(REGISTRY.devices().is_empty());
    assert_eq!(first.removes.load(Ordering::Relaxed), 2);
    assert_eq!(second.removes.load(Ordering::Relaxed), 2);
    assert!(!first.cleanup_failed.load(Ordering::Relaxed));
    assert!(!second.cleanup_failed.load(Ordering::Relaxed));
    assert_eq!(first.ops_drops.load(Ordering::Relaxed), 1);
    assert_eq!(second.ops_drops.load(Ordering::Relaxed), 1);
    assert_eq!(retained_ops_drops, (1, 1));
    assert_eq!(retained_names, (Some("driver-a"), Some("driver-b")));
    assert_eq!(drops_before_release, (0, 0));
    assert_eq!(first.runtime_drops.load(Ordering::Relaxed), 2);
    assert_eq!(second.runtime_drops.load(Ordering::Relaxed), 2);
    assert_eq!(*first.events.lock(), first_events);
    assert_eq!(*second.events.lock(), second_events);
}
