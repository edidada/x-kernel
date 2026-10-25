// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Controller-owned downstream teardown using real model management locks.
//!
//! The enumeration gate is independent of the parent service transaction lock.
//! Child callbacks never acquire the enumeration gate. Completion handshakes
//! establish ordering; bounded waits only diagnose stuck-task failures.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, Ordering};

use driver_model::{
    AddDeviceError, Bus, BusInstanceId, DeviceAccess, DeviceDescriptor, DeviceDomain, DeviceRef,
    DeviceState, DriverDesc, DriverOps, Environment, OfMatch, PlatformBus, PlatformBusData,
    PlatformId, ProbeError, RuntimeError,
};
use kcompletion::Completion;
use ksync::Mutex;
use unittest::{assert, assert_eq, def_test};

use super::device_management_lifecycle_unittest::{HostServices, join_bounded, wait_bounded};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    ChildRemove(bool),
    ChildDrop,
    ParentStop,
    ParentDrop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitError {
    Closed,
    DomainGone,
    Add(AddDeviceError),
}

struct Enumeration {
    is_open: bool,
    owned: Vec<DeviceRef>,
}

struct ChildManager {
    domain: Weak<DeviceDomain>,
    parent: DeviceRef,
    gate: Mutex<Enumeration>,
    service_is_live: Mutex<bool>,
    events: Arc<Mutex<Vec<Event>>>,
    stop_entered: Completion,
    children_removed: AtomicBool,
}

impl ChildManager {
    fn commit(&self, node: u32) -> Result<DeviceRef, CommitError> {
        let mut gate = self.gate.lock();
        if !gate.is_open {
            return Err(CommitError::Closed);
        }
        let domain = self.domain.upgrade().ok_or(CommitError::DomainGone)?;
        let child = domain
            .add_device(descriptor(
                self.parent.descriptor().bus_instance,
                node,
                "test,owned-child",
                Some(self.parent.clone()),
            ))
            .map_err(CommitError::Add)?;
        gate.owned.push(child.clone());
        Ok(child)
    }

    fn shutdown(&self) {
        self.stop_entered.complete();
        let children = {
            let mut gate = self.gate.lock();
            gate.is_open = false;
            core::mem::take(&mut gate.owned)
        };
        let mut all_removed = true;
        if let Some(domain) = self.domain.upgrade() {
            for child in children {
                all_removed &= domain.remove_device(&child).is_ok();
            }
        } else {
            all_removed = false;
        }
        self.children_removed.store(all_removed, Ordering::Release);
        *self.service_is_live.lock() = false;
        self.events.lock().push(Event::ParentStop);
    }
}

struct PausedProbe {
    entered: Completion,
    release: Completion,
    was_released: AtomicBool,
}

struct ReleaseProbeOnDrop(Arc<PausedProbe>);

impl Drop for ReleaseProbeOnDrop {
    fn drop(&mut self) {
        self.0.release.complete_all();
    }
}

struct Shared {
    domain: Weak<DeviceDomain>,
    current: Mutex<Option<Weak<ChildManager>>>,
    pause: Mutex<Option<Arc<PausedProbe>>>,
    events: Arc<Mutex<Vec<Event>>>,
}

struct ControllerDriver(Arc<Shared>);
struct ControllerData(Arc<ChildManager>);

impl Drop for ControllerData {
    fn drop(&mut self) {
        self.0.events.lock().push(Event::ParentDrop);
    }
}

impl DriverOps<PlatformBus> for ControllerDriver {
    type Data = ControllerData;

    fn probe(
        &self,
        device: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
    ) -> Result<ControllerData, ProbeError> {
        let manager = Arc::new(ChildManager {
            domain: self.0.domain.clone(),
            parent: device.reference(),
            gate: Mutex::new(Enumeration {
                is_open: true,
                owned: Vec::new(),
            }),
            service_is_live: Mutex::new(true),
            events: self.0.events.clone(),
            stop_entered: Completion::new(),
            children_removed: AtomicBool::new(false),
        });
        *self.0.current.lock() = Some(Arc::downgrade(&manager));
        Ok(ControllerData(manager))
    }

    fn remove(
        &self,
        _: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
        data: &mut ControllerData,
    ) {
        data.0.shutdown();
    }
}

struct ChildDriver(Arc<Shared>);
struct ChildData(Arc<ChildManager>);

impl Drop for ChildData {
    fn drop(&mut self) {
        self.0.events.lock().push(Event::ChildDrop);
    }
}

impl DriverOps<PlatformBus> for ChildDriver {
    type Data = ChildData;

    fn probe(
        &self,
        _: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
    ) -> Result<ChildData, ProbeError> {
        let manager = self
            .0
            .current
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(RuntimeError::NotFound)?;
        if !*manager.service_is_live.lock() {
            return Err(RuntimeError::BadState.into());
        }
        let pause = self.0.pause.lock().clone();
        if let Some(pause) = pause {
            pause.entered.complete();
            pause.was_released.store(
                wait_bounded(|| pause.release.is_completed()),
                Ordering::Release,
            );
        }
        Ok(ChildData(manager))
    }

    fn remove(
        &self,
        _: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
        data: &mut ChildData,
    ) {
        let was_live = data
            .0
            .service_is_live
            .try_lock()
            .is_some_and(|service| *service);
        data.0.events.lock().push(Event::ChildRemove(was_live));
    }
}

fn descriptor(
    root: BusInstanceId,
    node: u32,
    compatible: &str,
    parent: Option<DeviceRef>,
) -> DeviceDescriptor {
    DeviceDescriptor::builder()
        .bus_instance(root)
        .bus_data(Box::new(PlatformBusData::firmware_node(
            node,
            &[compatible],
        )))
        .parent_opt(parent)
        .build()
}

struct Fixture {
    domain: Arc<DeviceDomain>,
    shared: Arc<Shared>,
    root: BusInstanceId,
    can_cleanup: bool,
}

impl Fixture {
    fn new() -> Result<Self, AddDeviceError> {
        let domain = Arc::new(DeviceDomain::new());
        domain.set_services(Arc::new(HostServices::new()));
        let root = domain.add_bus_instance(PlatformBus::ID, None, "downstream-ut")?;
        let shared = Arc::new(Shared {
            domain: Arc::downgrade(&domain),
            current: Mutex::new(None),
            pause: Mutex::new(None),
            events: Arc::new(Mutex::new(Vec::new())),
        });
        static CONTROLLER_IDS: &[PlatformId] =
            &[PlatformId::Of(OfMatch::compatible("test,controller"))];
        static CHILD_IDS: &[PlatformId] =
            &[PlatformId::Of(OfMatch::compatible("test,owned-child"))];
        domain.register_driver(DriverDesc::new(
            "controller-ut",
            CONTROLLER_IDS,
            ControllerDriver(shared.clone()),
        ));
        domain.register_driver(DriverDesc::new(
            "child-ut",
            CHILD_IDS,
            ChildDriver(shared.clone()),
        ));
        Ok(Self {
            domain,
            shared,
            root,
            can_cleanup: true,
        })
    }

    fn manager(&self) -> Option<Arc<ChildManager>> {
        self.shared.current.lock().as_ref().and_then(Weak::upgrade)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(pause) = self.shared.pause.lock().take() {
            pause.release.complete_all();
        }
        if self.can_cleanup {
            for snapshot in self.domain.devices() {
                let _ = self.domain.remove_device(&snapshot.device);
            }
        }
    }
}

#[def_test(serial)]
fn controller_removes_only_owned_children_before_stopping_parent_services() {
    let fixture = Fixture::new().unwrap();
    let parent = fixture
        .domain
        .add_device(descriptor(fixture.root, 1, "test,controller", None))
        .unwrap();
    let manager = fixture.manager().unwrap();
    let owned = manager.commit(2).unwrap();
    let outsider = fixture
        .domain
        .add_device(descriptor(
            fixture.root,
            3,
            "test,unowned",
            Some(parent.clone()),
        ))
        .unwrap();
    assert_eq!(fixture.domain.remove_device(&parent), Ok(()));
    assert!(fixture.domain.snapshot(&parent).is_none());
    assert!(fixture.domain.snapshot(&owned).is_none());
    assert_eq!(
        fixture.domain.snapshot(&outsider).unwrap().state,
        DeviceState::Unclaimed
    );
    assert!(
        outsider
            .descriptor()
            .parent
            .as_ref()
            .is_some_and(|reference| reference.same_device(&parent))
    );
    assert!(manager.children_removed.load(Ordering::Acquire));
    let events = fixture.shared.events.lock().clone();
    assert_eq!(
        events.as_slice(),
        &[
            Event::ChildRemove(true),
            Event::ChildDrop,
            Event::ParentStop,
            Event::ParentDrop,
        ]
    );
    assert_eq!(fixture.domain.remove_device(&outsider), Ok(()));
    assert!(fixture.domain.devices().is_empty());
}

#[def_test(serial)]
fn enumeration_commit_is_drained_and_old_binding_cannot_publish_after_rebind() {
    let mut fixture = Fixture::new().unwrap();
    let parent = fixture
        .domain
        .add_device(descriptor(fixture.root, 1, "test,controller", None))
        .unwrap();
    let old = fixture.manager().unwrap();
    let late_binding = old.clone();
    let pause = Arc::new(PausedProbe {
        entered: Completion::new(),
        release: Completion::new(),
        was_released: AtomicBool::new(false),
    });
    *fixture.shared.pause.lock() = Some(pause.clone());
    let release_on_drop = ReleaseProbeOnDrop(pause.clone());
    let committed = Arc::new(Mutex::new(None));
    let enumeration = ktask::spawn({
        let old = old.clone();
        let committed = committed.clone();
        move || {
            let result = old.commit(2);
            *committed.lock() = Some(result);
        }
    });
    let probe_entered = wait_bounded(|| pause.entered.try_wait());
    let exit_result = Arc::new(Mutex::new(None));
    let exiting = ktask::spawn({
        let domain = fixture.domain.clone();
        let parent = parent.clone();
        let exit_result = exit_result.clone();
        move || {
            let result = domain.unbind_device(&parent);
            *exit_result.lock() = Some(result);
        }
    });
    let stop_entered = wait_bounded(|| old.stop_entered.try_wait());
    let exit_blocked = wait_bounded(|| {
        exiting.state() == ktask::TaskState::Blocked || exiting.state() == ktask::TaskState::Exited
    }) && exiting.state() == ktask::TaskState::Blocked;
    let service_live_before_release = *old.service_is_live.lock();
    drop(release_on_drop);
    let exits = [join_bounded(&enumeration), join_bounded(&exiting)];
    fixture.can_cleanup = exits == [Some(0), Some(0)];
    *fixture.shared.pause.lock() = None;
    assert!(probe_entered && stop_entered && exit_blocked);
    assert!(service_live_before_release);
    assert!(pause.was_released.load(Ordering::Acquire));
    assert_eq!(exits, [Some(0), Some(0)]);
    assert_eq!(*exit_result.lock(), Some(Ok(())));
    let child = committed.lock().take().unwrap().unwrap();
    assert!(fixture.domain.snapshot(&child).is_none());
    assert!(old.children_removed.load(Ordering::Acquire));
    let events = fixture.shared.events.lock().clone();
    assert_eq!(
        events.as_slice(),
        &[
            Event::ChildRemove(true),
            Event::ChildDrop,
            Event::ParentStop,
            Event::ParentDrop,
        ]
    );
    assert_eq!(
        fixture.domain.snapshot(&parent).unwrap().state,
        DeviceState::Unclaimed
    );
    assert_eq!(fixture.domain.bind_device(&parent).unwrap().activated, 1);
    let new = fixture.manager().unwrap();
    assert!(!Arc::ptr_eq(&old, &new));
    let registered_before_late_commit = fixture.domain.devices().len();
    assert!(matches!(late_binding.commit(3), Err(CommitError::Closed)));
    assert_eq!(
        fixture.domain.devices().len(),
        registered_before_late_commit
    );
    assert!(!*old.service_is_live.lock());
    let fresh = new.commit(4).unwrap();
    assert_eq!(
        fixture.domain.snapshot(&fresh).unwrap().state,
        DeviceState::Active
    );
    assert_eq!(fixture.domain.remove_device(&parent), Ok(()));
    assert!(new.children_removed.load(Ordering::Acquire));
    assert!(fixture.domain.snapshot(&fresh).is_none());
    assert!(fixture.domain.devices().is_empty());
    let events = fixture.shared.events.lock().clone();
    assert_eq!(
        events.as_slice(),
        &[
            Event::ChildRemove(true),
            Event::ChildDrop,
            Event::ParentStop,
            Event::ParentDrop,
            Event::ChildRemove(true),
            Event::ChildDrop,
            Event::ParentStop,
            Event::ParentDrop,
        ]
    );
}
