// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Probe dispatch and binding under each device's management lock.

use super::*;

impl DeviceDomain {
    /// Drain queued probes without holding a domain-wide callback lock.
    ///
    /// Rejected management entries remain pending and are counted in
    /// `ProbeStats::rejected`. Explicit management callers should use
    /// `bind_device`, which returns the entry error directly.
    /// Taking a batch under the domain lock claims those queue entries. There
    /// is no global executor ownership to release: a producer arriving after
    /// an empty observation dispatches its own work. Each candidate is checked
    /// again under its management lock, including candidates already dequeued
    /// when another caller unregistered or bound their device.
    pub fn probe_pending(&self) -> ProbeStats {
        let mut stats = ProbeStats::default();
        let mut rejected = Vec::new();
        loop {
            let batch = {
                let mut inner = self.inner.lock();
                let pending = core::mem::take(&mut inner.sched.pending);
                pending
                    .into_iter()
                    .filter_map(|id| {
                        inner.devices.get(&id).map(|entry| crate::DeviceRef {
                            core: entry.core.clone(),
                        })
                    })
                    .collect::<Vec<_>>()
            };
            #[cfg(unittest)]
            {
                let hook_fn = self.dispatch_hook_fn.lock().take();
                if let Some(hook_fn) = hook_fn {
                    hook_fn(self, batch.is_empty());
                }
            }
            if batch.is_empty() {
                break;
            }
            for device in batch {
                let result =
                    self.with_management(&device, || self.probe_device_locked(&device, &mut stats));
                match result {
                    Err(RuntimeError::Management(_)) => {
                        stats.rejected += 1;
                        rejected.push(device);
                    }
                    Err(RuntimeError::NotFound) | Ok(()) => {}
                    Err(_) => stats.failed += 1,
                }
            }
        }
        let mut inner = self.inner.lock();
        for device in &rejected {
            let id = device.core.id.raw();
            if inner.devices.get(&id).is_some_and(|entry| {
                Arc::ptr_eq(&entry.core, &device.core)
                    && entry.lifecycle.state == DeviceState::Pending
            }) && !inner.sched.pending.contains(&id)
            {
                inner.sched.pending.push(id);
            }
        }
        stats
    }

    /// Attempt a fresh binding while retaining the existing registration.
    ///
    /// No match or deferred probing is not a registration failure. Already
    /// bound devices are unchanged. Unregistered or foreign objects are
    /// rejected; context and reentry errors occur before changing state.
    pub fn bind_device(&self, device: &crate::DeviceRef) -> Result<ProbeStats, RuntimeError> {
        self.with_management(device, || {
            let id = device.core.id.raw();
            {
                let mut inner = self.inner.lock();
                let entry = inner
                    .devices
                    .get_mut(&id)
                    .filter(|entry| Arc::ptr_eq(&entry.core, &device.core))
                    .ok_or(RuntimeError::NotFound)?;
                if entry.lifecycle.state == DeviceState::Bound {
                    return Ok(ProbeStats::default());
                }
                entry.lifecycle.state = DeviceState::Pending;
                entry.binding.attempted.clear();
                entry.probe.last_deferred_dep = None;
                inner.sched.pending.retain(|pending| *pending != id);
                inner.sched.deferred.retain(|(pending, _)| *pending != id);
            }
            let mut stats = ProbeStats::default();
            loop {
                self.probe_device_locked(device, &mut stats)?;
                let inner = self.inner.lock();
                if !inner
                    .devices
                    .get(&id)
                    .is_some_and(|entry| entry.lifecycle.state == DeviceState::Pending)
                {
                    break;
                }
            }
            Ok(stats)
        })
    }

    fn probe_device_locked(
        &self,
        device: &crate::DeviceRef,
        stats: &mut ProbeStats,
    ) -> Result<(), RuntimeError> {
        let id = device.core.id.raw();
        let (drivers, attempted, bus, services, driver_generation) = {
            let mut inner = self.inner.lock();
            let entry = inner
                .devices
                .get(&id)
                .filter(|entry| Arc::ptr_eq(&entry.core, &device.core))
                .ok_or(RuntimeError::NotFound)?;
            if entry.lifecycle.state != DeviceState::Pending {
                return Ok(());
            }
            let selected = (
                inner.drivers.clone(),
                entry.binding.attempted.clone(),
                bus_type(&inner.bus_instances, &device.core.desc),
                entry.services.clone(),
                inner.next_driver_key,
            );
            inner.sched.pending.retain(|pending| *pending != id);
            selected
        };
        let candidate = drivers
            .iter()
            .filter(|driver| !attempted.contains(&driver.key()))
            .find(|driver| driver.matches(bus, device.core.desc.bus_data.as_ref()))
            .cloned();
        {
            let mut inner = self.inner.lock();
            let registry_changed = inner.next_driver_key != driver_generation;
            let entry = inner.devices.get_mut(&id).ok_or(RuntimeError::NotFound)?;
            entry.lifecycle.state = if candidate.is_some() {
                DeviceState::Probing
            } else if registry_changed {
                DeviceState::Pending
            } else {
                DeviceState::Unclaimed
            };
            if candidate.is_none() && registry_changed {
                inner.sched.pending.push(id);
            }
        }
        let Some(driver) = candidate else {
            return Ok(());
        };
        match driver.probe(&device.core, services.as_ref()) {
            Ok(data) => {
                let mut inner = self.inner.lock();
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: the device management lock \
                              spans probe and commit; unregister/unbind require that lock and \
                              same-task reentry is rejected; scheduling only changes Pending, \
                              Deferred or Unclaimed entries, never this Probing registration"
                )]
                let entry = inner
                    .devices
                    .get_mut(&id)
                    .filter(|entry| {
                        Arc::ptr_eq(&entry.core, &device.core)
                            && entry.lifecycle.state == DeviceState::Probing
                            && entry.binding.driver.is_none()
                            && entry.binding.data.is_none()
                    })
                    .expect("management lock preserves the unbound probing registration");
                entry.binding.driver = Some(driver);
                entry.binding.data = Some(data);
                entry.lifecycle.state = DeviceState::Bound;
                stats.activated += 1;
            }
            Err(ProbeError::Deferred(dep)) => {
                device.core.clear_driver();
                self.park_deferred(&mut self.inner.lock(), id, dep, stats, driver.name());
            }
            Err(ProbeError::Fatal(error)) => {
                device.core.clear_driver();
                Self::record_failure(&mut self.inner.lock(), id, &driver);
                stats.failed += 1;
                let level = match error {
                    RuntimeError::Unsupported => log::Level::Debug,
                    _ => log::Level::Warn,
                };
                log::log!(
                    level,
                    "driver-core: probe failed: device={device:?} path={} driver={} \
                     error={error:?}",
                    device.path(),
                    driver.name(),
                );
            }
        }
        Ok(())
    }
}
