// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Removal orchestration: the exclusive claim protocol (retryable vs
//! in-progress), the children-first cascade (device children ∪ bridged
//! bus-instance members) and the teardown phases
//! (quiesce → Data drop → Removed).

use super::*;

impl DeviceDomain {
    /// Tear a device down in the Linux order: `remove()` (quiesce), then the
    /// remove notifier (class publications withdrawn and their consumers
    /// detached — while the hardware is still mapped), then drop `Data`
    /// (IRQ release, transport teardown, MMIO unmap last), then mark removed.
    ///
    /// A device that never reached `Active` (Pending/Deferred/Probing/
    /// Unclaimed — e.g. hotplug of an unmatched card) takes the light path:
    /// it is withdrawn from the pending/deferred worklists and its entry is
    /// freed immediately; there is no binding to quiesce. `Removing`/`
    /// `Removed` entries reject re-removal with `BadState`.
    pub fn remove_device(&self, id: DeviceId) -> Result<(), RuntimeError> {
        // Three phases, with the topology entrance CLOSED first:
        //
        // 1. Under one lock: validate, flip to `Removing` (this stops
        //    add_device from attaching new children — the cascade snapshot
        //    below cannot see an entry added after it), bump the binding
        //    generation, and snapshot the children. The binding's
        //    driver/data are NOT taken yet.
        // 2. Outside the lock: cascade children-first (a child's driver
        //    may hold resources referencing this device's hardware).
        //    ANY child failure aborts: the parent stays `Removing` —
        //    closed to new children, binding intact — and the error is
        //    returned for retry. A child mid-probe (Busy) or mid-teardown
        //    (BadState) MUST block the parent's teardown: proceeding
        //    would free the parent's resources under the child's feet.
        // 3. Retake the lock (only this flow transitions Removing forward
        //    for this id), take driver/data, and run the teardown:
        //    remove() → drop(Data).
        //
        // (Linux expresses the same ordering through the bus driver's own
        // remove walk; the device model here owns the link.)
        let (children, light_path) = {
            let mut inner = self.inner.lock();
            let Some(entry) = inner.devices.get_mut(&id.raw()) else {
                return Err(RuntimeError::NotFound);
            };
            match entry.lifecycle.state {
                // A fresh claim: flip to `Removing` (closed to new
                // children), bump the generation, snapshot. `light` is
                // whether a binding exists to tear down — recorded here
                // because the resume path re-derives it from the same
                // field.
                DeviceState::Active
                | DeviceState::Pending
                | DeviceState::Deferred
                | DeviceState::Unclaimed => {
                    let light = entry.binding.driver.is_none();
                    entry.lifecycle.state = DeviceState::Removing;
                    entry.lifecycle.generation += 1;
                    entry.lifecycle.remove_retryable = false;
                    (Self::teardown_set(&inner, &id.raw()), light)
                }
                // Resuming an ABORTED cascade: only the caller that
                // atomically claims the flag under THIS lock proceeds —
                // concurrent removers see `Busy`. A removal genuinely in
                // progress (retryable == false) is owned by its caller.
                DeviceState::Removing if entry.lifecycle.remove_retryable => {
                    let light = entry.binding.driver.is_none();
                    entry.lifecycle.remove_retryable = false;
                    (Self::teardown_set(&inner, &id.raw()), light)
                }
                DeviceState::Removing => {
                    return Err(RuntimeError::Busy);
                }
                DeviceState::Removed => {
                    return Err(RuntimeError::BadState);
                }
                // A probe is in flight on another thread: its side effects
                // (class publication, IRQ registration, provider publish)
                // are happening NOW. Removing the entry here would orphan
                // every one of those — the probe has no owner to hand them
                // back to. Reject with Busy; retry after probe completes.
                DeviceState::Probing => {
                    return Err(RuntimeError::Busy);
                }
            }
        };
        if let Err(err) = self.cascade(children) {
            Self::mark_remove_retryable(self, id.raw());
            return Err(err);
        }
        if light_path {
            let mut inner = self.inner.lock();
            Self::unlink_from_parent(&mut inner, &id.raw());
            inner.devices.remove(&id.raw());
            inner.sched.pending.retain(|pending| *pending != id.raw());
            inner
                .sched
                .deferred
                .retain(|(deferred, _)| *deferred != id.raw());
            log::debug!("driver-core: removed unbound device {id} (no binding to tear down)");
            return Ok(());
        }
        let (desc, driver, core, data, services) = {
            let mut inner = self.inner.lock();
            let Some(entry) = inner.devices.get_mut(&id.raw()) else {
                return Err(RuntimeError::NotFound);
            };
            if entry.lifecycle.state != DeviceState::Removing {
                return Err(RuntimeError::BadState);
            }
            (
                entry.desc.clone(),
                entry.binding.driver.take(),
                entry.core.clone(),
                entry.binding.data.take(),
                inner.services.clone(),
            )
        };
        let (driver, mut data, services) = (driver, data, services);

        if let (Some(driver), Some(data)) = (driver.as_ref(), data.as_mut()) {
            driver.remove(&desc, id, &core, data.as_mut(), services.as_ref());
        }

        // `device_removed` fires AFTER the final state flip — the name
        // now means what it says (registry no longer lists the device).
        // The publications are Data-owned: they withdraw inside
        // `drop(data)` below, sequenced by each Data's field order.
        drop(data);
        {
            let mut inner = self.inner.lock();
            Self::unlink_from_parent(&mut inner, &id.raw());
            if let Some(entry) = inner.devices.get_mut(&id.raw()) {
                entry.lifecycle.state = DeviceState::Removed;
            }
        }
        Ok(())
    }

    /// Phase 2: remove each cascaded member, children-first. A member
    /// already gone (prior pass / concurrent remove) counts as done; any
    /// other failure aborts the teardown for a retry.
    pub(super) fn cascade(&self, members: Vec<u64>) -> Result<(), RuntimeError> {
        for member in members {
            match self.remove_device(DeviceId(member)) {
                Ok(()) => {}
                // Already gone: satisfies the children-first requirement.
                Err(RuntimeError::NotFound) => {}
                Err(RuntimeError::BadState) => {
                    let member_gone = {
                        let inner = self.inner.lock();
                        matches!(
                            inner
                                .devices
                                .get(&member)
                                .map(|entry| entry.lifecycle.state),
                            Some(DeviceState::Removed) | None
                        )
                    };
                    if !member_gone {
                        log::warn!(
                            "driver-core: cascade remove of {member} hit BadState mid-teardown; \
                             teardown ABORTED — retry to continue"
                        );
                        return Err(RuntimeError::BadState);
                    }
                }
                // Busy (member probing) or any other failure: the member
                // still holds (or is acquiring) resources against this
                // device — abort for a retry.
                Err(err) => {
                    log::warn!(
                        "driver-core: cascade remove of {member} failed ({err:?}); teardown \
                         ABORTED — retry to continue"
                    );
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    /// The devices a removal must cascade through: device children PLUS
    /// the members of every bus instance this device bridges (Linux
    /// bridge-eject: `pci_stop_and_remove_bus_device` walks the secondary
    /// bus). Deduplicated, child-first order preserved within each group.
    pub(super) fn teardown_set(inner: &DomainInner, id: &u64) -> Vec<u64> {
        let mut set: Vec<u64> = inner
            .devices
            .get(id)
            .map(|entry| entry.topology.children.clone())
            .unwrap_or_default();
        for (instance_id, instance) in inner.bus_instances.iter() {
            if instance.bridge.as_ref() == Some(id) {
                for (member, entry) in inner.devices.iter() {
                    if entry.desc.bus_instance.map(|b| b.raw()) == Some(*instance_id)
                        && !set.contains(member)
                    {
                        set.push(*member);
                    }
                }
            }
        }
        set
    }

    /// Mark an aborted removal as claimable: the entry stays `Removing`
    /// (closed to new children) but the next `remove_device` caller may
    /// exclusively take over the resume.
    pub(super) fn mark_remove_retryable(domain: &DeviceDomain, id: u64) {
        if let Some(entry) = domain.inner.lock().devices.get_mut(&id) {
            entry.lifecycle.remove_retryable = true;
        }
    }

    /// Remove `id` from its parent's children list (topology unlink).
    fn unlink_from_parent(inner: &mut DomainInner, id: &u64) {
        let Some(parent) = inner.devices.get(id).map(|entry| entry.desc.parent) else {
            return;
        };
        let Some(parent) = parent else { return };
        let parent = parent.raw();
        if let Some(parent_entry) = inner.devices.get_mut(&parent) {
            parent_entry.topology.children.retain(|child| child != id);
        }
    }

    pub fn snapshot(&self, id: DeviceId) -> Option<DeviceSnapshot> {
        let inner = self.inner.lock();
        let entry = inner.devices.get(&id.raw())?;
        Some(snapshot_of(id, entry))
    }

    pub fn devices(&self) -> Vec<DeviceSnapshot> {
        let inner = self.inner.lock();
        inner
            .devices
            .iter()
            .map(|(id, entry)| snapshot_of(DeviceId(*id), entry))
            .collect()
    }

    /// Borrow a device's driver data under the domain lock (read-only view
    /// for class layers and tests).
    pub fn with_data<D: Any + Send + Sync, R>(
        &self,
        id: DeviceId,
        visit: impl FnOnce(&D) -> R,
    ) -> Option<R> {
        let inner = self.inner.lock();
        let entry = inner.devices.get(&id.raw())?;
        let data = entry.binding.data.as_ref()?.as_ref().downcast_ref::<D>()?;
        Some(visit(data))
    }
}
