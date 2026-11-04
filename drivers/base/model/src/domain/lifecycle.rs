// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-device binding teardown and registration removal.
//!
//! Management locks serialize callbacks for each device. The domain lock
//! protects short state changes only; driver callbacks and Data destruction
//! run outside it. Buses own downstream teardown and lock ordering.

use super::*;

impl DeviceDomain {
    /// Enter this device's management lock without holding the domain lock.
    ///
    /// Operations must revalidate the entry's object identity after entry.
    /// Foreign domains are rejected before invoking the host lock.
    pub(super) fn with_management<R>(
        &self,
        device: &crate::DeviceRef,
        operation: impl FnOnce() -> Result<R, RuntimeError>,
    ) -> Result<R, RuntimeError> {
        if !Arc::ptr_eq(&self.identity, &device.core.domain) {
            return Err(RuntimeError::NotFound);
        }
        let mut operation = Some(operation);
        let mut result = None;
        device.core.management.run_locked(&mut || {
            if let Some(operation) = operation.take() {
                result = Some(operation());
            }
        })?;
        result.unwrap_or(Err(RuntimeError::BadState))
    }

    /// End the current binding while preserving registration and identity.
    ///
    /// Pending and deferred work is cancelled. A bound driver's remove callback
    /// runs before its Data is dropped, with neither under the domain lock.
    /// The device returns to `Unclaimed` and may be explicitly rebound.
    /// An unbound device needs no driver callback.
    ///
    /// Requires sleepable management context and rejects same-device reentry.
    /// Foreign or unregistered objects return `NotFound`.
    /// Buses must stop their own enumeration and manage their downstream devices.
    pub fn unbind_device(&self, device: &crate::DeviceRef) -> Result<(), RuntimeError> {
        self.with_management(device, || self.unbind_locked(device, false))
    }

    /// End the binding and unregister this exact device object.
    ///
    /// The device becomes offline before driver teardown. Its registration
    /// and pending work are removed, and subsequent snapshots return `None`.
    /// Retained references keep identity alive but cannot bind again.
    /// Repeating removal of an already unregistered object in this domain
    /// succeeds without repeating cleanup. Foreign or mismatched objects
    /// return `NotFound`.
    ///
    /// Requires sleepable management context and rejects same-device reentry.
    /// No children or controller-owned bus members are removed automatically:
    /// their owning bus must arrange downstream teardown and lock ordering.
    pub fn remove_device(&self, device: &crate::DeviceRef) -> Result<(), RuntimeError> {
        self.with_management(device, || {
            let id = device.core.id.raw();
            {
                let inner = self.inner.lock();
                match inner.devices.get(&id) {
                    Some(entry) if Arc::ptr_eq(&entry.core, &device.core) => {}
                    None if !device.core.is_online() => return Ok(()),
                    _ => return Err(RuntimeError::NotFound),
                }
            }
            self.unbind_locked(device, true)?;
            let removed = {
                let mut inner = self.inner.lock();
                let entry = inner
                    .devices
                    .get(&id)
                    .filter(|entry| Arc::ptr_eq(&entry.core, &device.core))
                    .ok_or(RuntimeError::NotFound)?;
                if entry.lifecycle.state != DeviceState::Removing {
                    return Err(RuntimeError::BadState);
                }
                Self::unlink_from_parent(&mut inner, &id);
                inner.devices.remove(&id)
            };
            drop(removed);
            Ok(())
        })
    }

    /// Drain a binding while the caller owns the management lock.
    ///
    /// Unregistration keeps the entry closed through deletion; ordinary
    /// unbinding reopens it only after remove and Data destruction complete.
    fn unbind_locked(
        &self,
        device: &crate::DeviceRef,
        unregister: bool,
    ) -> Result<(), RuntimeError> {
        let id = device.core.id.raw();
        let (driver, mut data, services) = {
            let mut inner = self.inner.lock();
            let entry = inner
                .devices
                .get_mut(&id)
                .filter(|entry| Arc::ptr_eq(&entry.core, &device.core))
                .ok_or(RuntimeError::NotFound)?;
            match entry.lifecycle.state {
                DeviceState::Pending
                | DeviceState::Deferred
                | DeviceState::Unclaimed
                | DeviceState::Active => {}
                DeviceState::Probing | DeviceState::Removing => {
                    return Err(RuntimeError::Busy);
                }
                DeviceState::Removed => return Err(RuntimeError::BadState),
            }
            entry.lifecycle.state = DeviceState::Removing;
            entry.probe.last_deferred_dep = None;
            if unregister {
                entry.core.mark_offline();
            }
            let driver = entry.binding.driver.take();
            let data = entry.binding.data.take();
            let services = entry.services.clone();
            inner.sched.pending.retain(|pending| *pending != id);
            inner.sched.deferred.retain(|(deferred, _)| *deferred != id);
            (driver, data, services)
        };

        if let (Some(driver), Some(data)) = (driver.as_ref(), data.as_mut()) {
            driver.remove(&device.core, data.as_mut(), services.as_ref());
        }
        drop(data);
        device.core.clear_driver();

        if !unregister {
            let mut inner = self.inner.lock();
            let entry = inner
                .devices
                .get_mut(&id)
                .filter(|entry| Arc::ptr_eq(&entry.core, &device.core))
                .ok_or(RuntimeError::NotFound)?;
            if entry.lifecycle.state != DeviceState::Removing {
                return Err(RuntimeError::BadState);
            }
            entry.lifecycle.state = DeviceState::Unclaimed;
        }
        Ok(())
    }

    /// Remove `id` from its parent's children list (topology unlink).
    fn unlink_from_parent(inner: &mut DomainInner, id: &u64) {
        let Some(parent) = inner.devices.get(id).and_then(|entry| {
            entry
                .core
                .desc
                .parent
                .as_ref()
                .map(|parent| parent.core.id.raw())
        }) else {
            return;
        };
        if let Some(parent_entry) = inner.devices.get_mut(&parent) {
            parent_entry.topology.children.retain(|child| child != id);
        }
    }

    /// Capture this domain's state for the exact referenced object.
    ///
    /// Foreign or unregistered objects return `None`. The retained identity
    /// does not freeze later state changes or grant access to binding resources.
    pub fn snapshot(&self, device: &crate::DeviceRef) -> Option<DeviceSnapshot> {
        let mut snapshot = {
            let inner = self.inner.lock();
            let entry = inner
                .devices
                .get(&device.core.id.raw())
                .filter(|entry| Arc::ptr_eq(&entry.core, &device.core))?;
            snapshot_of(entry, bus_type(&inner.bus_instances, &entry.core.desc))
        };
        snapshot.identity = Some(device.core.desc.bus_data.display_identity());
        Some(snapshot)
    }

    /// Capture currently registered entries, including unbound devices.
    pub fn devices(&self) -> Vec<DeviceSnapshot> {
        let mut snapshots: Vec<_> = {
            let inner = self.inner.lock();
            inner
                .devices
                .values()
                .map(|entry| snapshot_of(entry, bus_type(&inner.bus_instances, &entry.core.desc)))
                .collect()
        };
        for snapshot in &mut snapshots {
            snapshot.identity = Some(snapshot.device.core.desc.bus_data.display_identity());
        }
        snapshots
    }

    /// Visit a bound device's typed Data under its management lock.
    ///
    /// Requires sleepable context. The visitor runs without the domain lock
    /// and must not reenter management of this device or wait for work needing
    /// its management lock. Data is restored before releasing management.
    /// Entry errors, foreign or unbound devices, and type mismatches return
    /// `None`; the visitor is not called in those cases.
    pub fn with_data<D: Any + Send + Sync, R>(
        &self,
        device: &crate::DeviceRef,
        visit: impl FnOnce(&D) -> R,
    ) -> Option<R> {
        self.with_management(device, || {
            let data = {
                let mut inner = self.inner.lock();
                let entry = inner
                    .devices
                    .get_mut(&device.core.id.raw())
                    .filter(|entry| Arc::ptr_eq(&entry.core, &device.core))
                    .ok_or(RuntimeError::NotFound)?;
                if entry.lifecycle.state != DeviceState::Active {
                    return Err(RuntimeError::BadState);
                }
                entry.binding.data.take().ok_or(RuntimeError::NotFound)?
            };
            let result = data.as_ref().downcast_ref::<D>().map(visit);
            {
                let mut inner = self.inner.lock();
                let entry = inner
                    .devices
                    .get_mut(&device.core.id.raw())
                    .filter(|entry| Arc::ptr_eq(&entry.core, &device.core))
                    .ok_or(RuntimeError::NotFound)?;
                if entry.lifecycle.state != DeviceState::Active || entry.binding.data.is_some() {
                    return Err(RuntimeError::BadState);
                }
                entry.binding.data = Some(data);
            }
            result.ok_or(RuntimeError::NotFound)
        })
        .ok()
    }
}
