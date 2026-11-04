// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deterministic interleavings at dependency checks and dispatch handoff.

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::Environment;

type CheckHookFn = dyn Fn() + Send + Sync;

struct Progress {
    is_ready: AtomicBool,
    probes: AtomicUsize,
    removes: AtomicUsize,
    drops: AtomicUsize,
    check_hook_fn: SpinNoPreempt<Option<Arc<CheckHookFn>>>,
}

struct ProgressDriver(Arc<Progress>);
struct ProgressData(Arc<Progress>);

impl Drop for ProgressData {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::Relaxed);
    }
}

const DEPENDENCY: DepSource = DepSource::clk(0x72);

impl DriverOps<PlatformBus> for ProgressDriver {
    type Data = ProgressData;

    fn probe(
        &self,
        _: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
    ) -> Result<Self::Data, ProbeError> {
        self.0.probes.fetch_add(1, Ordering::Relaxed);
        let is_ready = self.0.is_ready.load(Ordering::Acquire);
        let hook_fn = self.0.check_hook_fn.lock().take();
        if let Some(hook_fn) = hook_fn {
            hook_fn();
        }
        if is_ready {
            Ok(ProgressData(self.0.clone()))
        } else {
            Err(ProbeError::Deferred(DEPENDENCY))
        }
    }

    fn remove(
        &self,
        _: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
        _: &mut Self::Data,
    ) {
        self.0.removes.fetch_add(1, Ordering::Relaxed);
    }
}

struct Fixture {
    domain: Arc<DeviceDomain>,
    progress: Arc<Progress>,
    root: BusInstanceId,
}

impl Fixture {
    fn new() -> Self {
        let (domain, _, root) = fresh_domain();
        let domain = Arc::new(domain);
        let progress = Arc::new(Progress {
            is_ready: AtomicBool::new(false),
            probes: AtomicUsize::new(0),
            removes: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            check_hook_fn: SpinNoPreempt::new(None),
        });
        domain.register_driver(DriverDesc::new(
            "progress-ut",
            IDS,
            ProgressDriver(progress.clone()),
        ));
        Self {
            domain,
            progress,
            root,
        }
    }

    fn add(&self) -> crate::DeviceRef {
        self.domain.add_device(test_descriptor(self.root)).unwrap()
    }

    fn ready(&self) {
        self.progress.is_ready.store(true, Ordering::Release);
    }

    fn probes(&self) -> usize {
        self.progress.probes.load(Ordering::Relaxed)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let dispatch = self.domain.dispatch_hook_fn.lock().take();
        let check = self.progress.check_hook_fn.lock().take();
        drop((dispatch, check));
        for snapshot in self.domain.devices() {
            let _ = self.domain.unregister_device(&snapshot.device);
        }
    }
}

#[def_test]
fn dependency_available_before_probe_binds_without_retry() {
    let fixture = Fixture::new();
    fixture.ready();
    assert_eq!(fixture.domain.wake(DEPENDENCY), 0);
    let device = fixture.add();
    assert_eq!(
        fixture.domain.snapshot(&device).unwrap().state,
        DeviceState::Bound
    );
    assert_eq!(fixture.probes(), 1);
    assert!(fixture.domain.deferred_report().is_empty());
}

#[def_test]
fn dependency_arriving_after_check_before_parking_retries_without_another_wake() {
    let fixture = Fixture::new();
    let domain = Arc::downgrade(&fixture.domain);
    let progress = Arc::downgrade(&fixture.progress);
    *fixture.progress.check_hook_fn.lock() = Some(Arc::new(move || {
        progress
            .upgrade()
            .unwrap()
            .is_ready
            .store(true, Ordering::Release);
        core::assert_eq!(domain.upgrade().unwrap().wake(DEPENDENCY), 0);
    }));
    let device = fixture.add();
    assert_eq!(
        fixture.domain.snapshot(&device).unwrap().state,
        DeviceState::Bound
    );
    assert_eq!(fixture.probes(), 2);
    assert!(fixture.domain.deferred_report().is_empty());
}

#[def_test]
fn dependency_arriving_after_parking_wakes_only_its_waiters() {
    let fixture = Fixture::new();
    let device = fixture.add();
    assert_eq!(
        fixture.domain.snapshot(&device).unwrap().state,
        DeviceState::Deferred
    );
    fixture.ready();
    assert_eq!(fixture.domain.wake(DepSource::clk(0x73)), 0);
    assert_eq!(fixture.probes(), 1);
    assert_eq!(
        fixture.domain.snapshot(&device).unwrap().state,
        DeviceState::Deferred
    );
    assert_eq!(fixture.domain.wake(DEPENDENCY), 1);
    assert_eq!(
        fixture.domain.snapshot(&device).unwrap().state,
        DeviceState::Bound
    );
    assert_eq!(fixture.probes(), 2);
    assert_eq!(fixture.domain.wake(DEPENDENCY), 0);
    assert_eq!(fixture.probes(), 2);
}

#[def_test]
fn enqueue_after_empty_dispatch_observation_is_driven_by_the_producer() {
    let fixture = Fixture::new();
    let device = fixture.add();
    fixture.ready();
    let observed = Arc::new(AtomicBool::new(false));
    *fixture.domain.dispatch_hook_fn.lock() = Some(Arc::new({
        let observed = observed.clone();
        let device = device.clone();
        move |domain, is_empty| {
            core::assert!(is_empty);
            core::assert_eq!(domain.wake(DEPENDENCY), 1);
            core::assert_eq!(domain.snapshot(&device).unwrap().state, DeviceState::Bound);
            observed.store(true, Ordering::Release);
        }
    }));
    assert_eq!(fixture.domain.probe_pending(), ProbeStats::default());
    assert!(observed.load(Ordering::Acquire));
    assert_eq!(fixture.probes(), 2);
    assert_eq!(
        fixture.domain.snapshot(&device).unwrap().state,
        DeviceState::Bound
    );
}

#[def_test]
fn registration_after_empty_dispatch_observation_needs_no_third_dispatch() {
    let fixture = Fixture::new();
    fixture.ready();
    let registered = Arc::new(SpinNoPreempt::new(None));
    *fixture.domain.dispatch_hook_fn.lock() = Some(Arc::new({
        let registered = registered.clone();
        let root = fixture.root;
        move |domain, is_empty| {
            core::assert!(is_empty);
            let device = domain.add_device(test_descriptor(root)).unwrap();
            core::assert_eq!(domain.snapshot(&device).unwrap().state, DeviceState::Bound);
            *registered.lock() = Some(device);
        }
    }));
    assert_eq!(fixture.domain.probe_pending(), ProbeStats::default());
    assert!(registered.lock().is_some());
    assert_eq!(fixture.probes(), 1);
}

#[def_test]
fn unregistered_dequeued_candidate_and_late_dependency_cannot_probe_again() {
    let fixture = Fixture::new();
    let device = fixture.add();
    fixture.ready();
    *fixture.domain.dispatch_hook_fn.lock() = Some(Arc::new({
        let device = device.clone();
        move |domain, is_empty| {
            core::assert!(!is_empty);
            core::assert_eq!(domain.unregister_device(&device), Ok(()));
            core::assert_eq!(domain.wake(DEPENDENCY), 0);
        }
    }));
    assert_eq!(fixture.domain.wake(DEPENDENCY), 1);
    assert!(fixture.domain.snapshot(&device).is_none());
    assert_eq!(fixture.probes(), 1);
    assert_eq!(fixture.progress.removes.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.progress.drops.load(Ordering::Relaxed), 0);
    assert_eq!(
        fixture.domain.bind_device(&device),
        Err(RuntimeError::NotFound)
    );
    assert_eq!(fixture.domain.probe_pending(), ProbeStats::default());
}

#[def_test]
fn explicit_binding_of_dequeued_candidate_does_not_probe_twice() {
    let fixture = Fixture::new();
    let device = fixture.add();
    fixture.ready();
    *fixture.domain.dispatch_hook_fn.lock() = Some(Arc::new({
        let device = device.clone();
        move |domain, is_empty| {
            core::assert!(!is_empty);
            core::assert_eq!(domain.bind_device(&device).unwrap().activated, 1);
        }
    }));
    assert_eq!(fixture.domain.wake(DEPENDENCY), 1);
    assert_eq!(
        fixture.domain.snapshot(&device).unwrap().state,
        DeviceState::Bound
    );
    assert_eq!(fixture.probes(), 2);
    assert_eq!(fixture.domain.unregister_device(&device), Ok(()));
    assert_eq!(fixture.progress.removes.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.progress.drops.load(Ordering::Relaxed), 1);
}
