// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The registries: devices, drivers, bus instances — registration,
//! validation (parent state, bus-instance type agreement), snapshots,
//! topology links, and the deferred-report view.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use kspin::SpinNoPreempt;

use super::*;
use crate::{
    bus::{Bus, BusId, BusInstanceId, DeviceDescriptor, DeviceId, MatchScore},
    device::DeviceServices,
    error::DepSource,
};

impl DeviceDomain {
    pub fn new() -> Self {
        Self {
            probing: SpinNoPreempt::new(false),
            inner: SpinNoPreempt::new(DomainInner {
                next_device_id: DEVICE_ID_ALLOCATOR.fetch_add(1, Ordering::Relaxed),
                bus_instances: BTreeMap::new(),
                sched: SchedulingState {
                    pending: Vec::new(),
                    deferred: Vec::new(),
                    dep_generations: Vec::new(),
                },
                drivers: Vec::new(),
                devices: BTreeMap::new(),
                services: Arc::new(NoServices),
                services_installed: false,
                observers: Vec::new(),
            }),
        }
    }

    /// Install the service bundle used by probes (MMIO/IRQ/DMA/clk). Must be
    /// called before probing; the clock registry wakes this domain.
    pub fn set_services(&self, services: Arc<dyn DeviceServices>) {
        let mut inner = self.inner.lock();
        inner.services = services;
        inner.services_installed = true;
    }

    /// Add a lifecycle observer. Observers are domain-owned (live until the
    /// domain does) and see every device removal — unlike the old
    /// single-slot remove notifier, multiple subsystems can subscribe.
    ///
    /// Class publications are NOT dispatched through observers anymore:
    /// each driver `Data` owns its publications (withdrawal guards, see
    /// `device_class::ClassWithdraw`) and unpublishes at its own drop. Observers
    /// are for subsystems that need removal visibility beyond publications
    /// (diagnostics, accounting, future hotplug policies).
    pub fn add_lifecycle_observer(&self, observer: Arc<dyn DeviceLifecycleObserver>) {
        self.inner.lock().observers.push(observer);
    }

    /// Register a driver and immediately probe anything waiting for it.
    pub fn register_driver<B: Bus, P: crate::driver::DriverOps<B>>(
        &self,
        desc: crate::driver::DriverDesc<B, P>,
    ) {
        {
            let mut inner = self.inner.lock();
            let name = desc.name;
            inner.drivers.push(Arc::new(RegisteredDriver { desc }));
            // Resweep: an Unclaimed device is a per-registration-generation
            // verdict, not a terminal one. A driver registered later (module
            // load, rebind) must be able to claim it; otherwise runtime
            // driver registration would silently miss every device that
            // exhausted its candidates during enumeration.
            let resweep: Vec<u64> = inner
                .devices
                .iter()
                .filter(|(_, entry)| entry.lifecycle.state == DeviceState::Unclaimed)
                .filter(|(_, entry)| {
                    inner.drivers.iter().any(|candidate| {
                        !entry.binding.attempted.contains(&driver_key(candidate))
                            && candidate.matches(&entry.desc) != MatchScore::NoMatch
                    })
                })
                .map(|(id, _)| *id)
                .collect();
            for id in resweep {
                if let Some(entry) = inner.devices.get_mut(&id) {
                    entry.lifecycle.state = DeviceState::Pending;
                }
                inner.sched.pending.push(id);
            }
            log::debug!(
                "driver-core: registered driver `{name}` for bus `{}`",
                B::ID.name()
            );
        }
        self.probe_pending();
    }

    /// Insert an enumeration product and probe it eagerly.
    /// Create a concrete bus instance (`pci_register_bus` /
    /// `i2c_add_adapter` shape): the addressing anchor for devices
    /// discovered on one physical bus. `bridge` names the controller
    /// device that created it (None for a root bus); removing that device
    /// tears the instance's members down with it.
    pub fn add_bus_instance(
        &self,
        bus: BusId,
        bridge: Option<DeviceId>,
        label: &str,
    ) -> Result<BusInstanceId, AddDeviceError> {
        let mut inner = self.inner.lock();
        if let Some(bridge) = bridge
            && !inner.devices.contains_key(&bridge.raw())
        {
            return Err(AddDeviceError::ParentMissing(bridge.raw()));
        }
        let id = BusInstanceId::from_raw(
            DEVICE_ID_ALLOCATOR.fetch_add(1, core::sync::atomic::Ordering::Relaxed),
        );
        inner.bus_instances.insert(
            id.raw(),
            BusInstanceEntry {
                bus,
                bridge: bridge.map(|b| b.raw()),
                label: alloc::string::String::from(label),
            },
        );
        Ok(id)
    }

    /// The devices attached to one bus instance, in id order — the
    /// `list_for_each_entry(bus->devices)` surface (instance-scoped
    /// rescan, eject, /proc listing).
    pub fn bus_instance_devices(&self, instance: BusInstanceId) -> Vec<DeviceId> {
        let inner = self.inner.lock();
        if !inner.bus_instances.contains_key(&instance.raw()) {
            return Vec::new();
        }
        inner
            .devices
            .iter()
            .filter(|(_, entry)| entry.desc.bus_instance == Some(instance))
            .map(|(id, _)| DeviceId(*id))
            .collect()
    }

    /// A snapshot of one instance's addressing identity: (bus type,
    /// bridge device, label).
    pub fn bus_instance_identity(
        &self,
        instance: BusInstanceId,
    ) -> Option<(BusId, Option<DeviceId>, alloc::string::String)> {
        let inner = self.inner.lock();
        inner
            .bus_instances
            .get(&instance.raw())
            .map(|entry| (entry.bus, entry.bridge.map(DeviceId), entry.label.clone()))
    }

    pub fn add_device(&self, desc: DeviceDescriptor) -> Result<DeviceId, AddDeviceError> {
        let id = {
            let mut inner = self.inner.lock();
            if !inner.services_installed {
                // NoServices rejects every service call as Fatal-Unsupported
                // and failed probes are never retried — a device added
                // before set_services is permanently lost. Make the
                // ordering violation loud instead of silent.
                log::error!(
                    "driver-core: device added before services were installed; probes will fail \
                     permanently — call set_services first"
                );
            }
            let id = DeviceId(inner.next_device_id);
            inner.next_device_id = DEVICE_ID_ALLOCATOR.fetch_add(1, Ordering::Relaxed);
            // Bus-instance validation, under the same lock: a stated
            // instance must exist and its bus TYPE must agree with the
            // descriptor's — "this PCI function sits on bus 0000:00"
            // cannot name a platform-bus instance.
            if let Some(instance) = desc.bus_instance {
                match inner.bus_instances.get(&instance.raw()) {
                    Some(entry) if entry.bus == desc.bus => {}
                    Some(entry) => {
                        return Err(AddDeviceError::BusInstanceTypeMismatch {
                            instance: instance.raw(),
                            instance_bus: entry.bus.name(),
                            device_bus: desc.bus.name(),
                        });
                    }
                    None => {
                        return Err(AddDeviceError::BusInstanceMissing(instance.raw()));
                    }
                }
            }
            // Topology validation, under the SAME lock that governs
            // removal: a stated parent must exist AND be accepting
            // children. A parent mid-teardown (Removing) or gone
            // (Removed) rejects the add — the cascade snapshot cannot see
            // an entry added after it, so accepting it would orphan the
            // child at the parent's teardown. A missing parent is an
            // error, not a silent root placement: the descriptor's own
            // `parent` field would disagree with the registry topology.
            if let Some(parent) = desc.parent {
                let parent_state = inner
                    .devices
                    .get(&parent)
                    .map(|entry| entry.lifecycle.state);
                match parent_state {
                    Some(
                        DeviceState::Pending
                        | DeviceState::Probing
                        | DeviceState::Active
                        | DeviceState::Deferred
                        | DeviceState::Unclaimed,
                    ) => {
                        #[expect(
                            clippy::expect_used,
                            reason = "CG-021-no-panic-on-reachable-failure: the parent state \\
                                      matched an accepting state above"
                        )]
                        {
                            inner
                                .devices
                                .get_mut(&parent)
                                .expect("parent present")
                                .topology
                                .children
                                .push(id.raw());
                        }
                    }
                    Some(state @ (DeviceState::Removing | DeviceState::Removed)) => {
                        return Err(AddDeviceError::ParentUnavailable {
                            parent,
                            state: state.as_str(),
                        });
                    }
                    None => return Err(AddDeviceError::ParentMissing(parent)),
                }
            }
            inner.devices.insert(
                id.raw(),
                DeviceEntry {
                    desc,
                    lifecycle: LifecycleState {
                        state: DeviceState::Pending,
                        generation: 0,
                        remove_retryable: false,
                    },
                    binding: BindingState {
                        driver: None,
                        data: None,
                        attempted: Vec::new(),
                    },
                    topology: TopologyState {
                        children: Vec::new(),
                    },
                    probe: ProbeState {
                        last_deferred_dep: None,
                        dep_seen_generations: Vec::new(),
                    },
                },
            );
            inner.sched.pending.push(id.raw());
            id
        };
        self.probe_pending();
        Ok(id)
    }

    /// One line per device still parked on a dependency: `(id, dep)`.
    ///
    /// The composition root logs this at boot end — Linux's
    /// `deferred_probe_timeout` diagnostics analogue, minus the timer: the
    /// root knows when enumeration + provider registration completed, so it
    /// decides when a deferred device is stuck rather than still waiting.
    pub fn deferred_report(&self) -> Vec<(u64, DepSource)> {
        self.inner.lock().sched.deferred.clone()
    }
}
