// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The probe engine: batch selection (highest-scoring unattempted driver
//! per device), execution outside the domain lock, and
//! generation-guarded commits (activation / deferred / fatal fallthrough).

use super::*;

impl DeviceDomain {
    /// Run the probe algorithm until no progress remains.
    pub fn probe_pending(&self) -> ProbeStats {
        {
            let mut probing = self.probing.lock();
            if *probing {
                // Another pump owns the queue; its pass covers devices
                // added before its current batch selection. Returning here
                // also guards the single-slot host resources (the MMIO
                // probe collector asserts serialization).
                return ProbeStats::default();
            }
            *probing = true;
        }
        let _guard = ProbePumpGuard { domain: self };
        self.probe_pending_locked()
    }

    pub(super) fn probe_pending_locked(&self) -> ProbeStats {
        let mut stats = ProbeStats::default();
        loop {
            let batch = self.select_batch();
            if batch.is_empty() {
                break stats;
            }
            for (id, desc, driver) in batch {
                let services = self.inner.lock().services.clone();
                let generation = self
                    .inner
                    .lock()
                    .devices
                    .get(&id)
                    .map_or(0, |e| e.lifecycle.generation);
                match driver.probe(&desc, crate::device::DeviceId(id), services.as_ref()) {
                    Ok(data) => {
                        let mut inner = self.inner.lock();
                        match inner.devices.get_mut(&id) {
                            Some(entry) if entry.lifecycle.generation == generation => {
                                entry.lifecycle.state = DeviceState::Active;
                                entry.binding.driver = Some(driver.clone());
                                entry.binding.data = Some(data);
                                stats.activated += 1;
                                log::debug!(
                                    "driver-core: `{}` activated device {id} ({})",
                                    driver.name(),
                                    desc.data.display_identity()
                                );
                            }
                            // Stale probe: the device's generation moved
                            // (torn down and re-added) while this probe
                            // ran. Its Data is not this entry's — drop it;
                            // the guards inside release against the
                            // services that created them.
                            _ => {
                                drop(data);
                                log::warn!(
                                    "driver-core: dropped stale probe result for device {id} \
                                     (generation moved)"
                                );
                            }
                        }
                    }
                    Err(ProbeError::Deferred(dep)) => {
                        let mut inner = self.inner.lock();
                        self.park_deferred(&mut inner, id, dep, &mut stats, driver.name());
                    }
                    Err(ProbeError::Fatal(err)) => {
                        let mut inner = self.inner.lock();
                        Self::record_failure(&mut inner, id, &driver);
                        stats.failed += 1;
                        log::debug!(
                            "driver-core: `{}` gave up on device {id}: {err}",
                            driver.name()
                        );
                    }
                }
            }
        }
    }

    fn select_batch(&self) -> Vec<(u64, DeviceDescriptor, Arc<dyn AnyDriver>)> {
        let mut inner = self.inner.lock();
        let mut batch = Vec::new();
        let pending = core::mem::take(&mut inner.sched.pending);
        for id in pending {
            let (state, attempted, desc) = {
                let Some(entry) = inner.devices.get_mut(&id) else {
                    continue;
                };
                (
                    entry.lifecycle.state,
                    entry.binding.attempted.clone(),
                    entry.desc.clone(),
                )
            };
            if matches!(
                state,
                DeviceState::Active | DeviceState::Removing | DeviceState::Removed
            ) {
                continue;
            }
            let best = inner
                .drivers
                .iter()
                .filter(|driver| !attempted.contains(&driver_key(driver)))
                .map(|driver| (driver.clone(), driver.matches(desc.data.as_ref())))
                .filter(|(_, score)| *score != MatchScore::NoMatch)
                .max_by_key(|(_, score)| *score)
                .map(|(driver, _)| driver);
            match best {
                Some(driver) => {
                    if let Some(entry) = inner.devices.get_mut(&id) {
                        entry.lifecycle.state = DeviceState::Probing;
                    }
                    batch.push((id, desc, driver));
                }
                None => {
                    if let Some(entry) = inner.devices.get_mut(&id) {
                        entry.lifecycle.state = DeviceState::Unclaimed;
                    }
                }
            }
        }
        batch
    }
}
