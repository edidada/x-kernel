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
                // Another pump owns the queue; our device is already in
                // `sched.pending`, but the running pump may have selected
                // its final (empty) batch — ask it for one more pass
                // before it releases the flag, or this device would sit
                // Pending with no trigger left. Returning here also guards
                // the single-slot host resources (the MMIO probe collector
                // asserts serialization).
                *self.pump_requested.lock() = true;
                return ProbeStats::default();
            }
            *probing = true;
        }
        let _guard = ProbePumpGuard { domain: self };
        self.probe_pending_locked()
    }

    pub(super) fn probe_pending_locked(&self) -> ProbeStats {
        let mut stats = ProbeStats::default();
        // Bounded: every pass either activates, defers, or fails a device, and a
        // wake re-queues the parked ones. That termination argument only holds
        // while wakes are finite — a provider published from inside a probe
        // wakes the consumers parked on it, and one of those consumers may
        // publish the next thing, which wakes the first again. Observed on
        // RK3588: the pump stopped returning altogether (the boot never printed
        // its device census and a phy that deferred 110 ms before its clock
        // appeared was never retried), which is indistinguishable from a hang
        // because it *is* one.
        //
        // A hard pass cap converts that livelock into a bounded boot plus a
        // report naming what is still waiting, so the system always makes
        // progress and the oscillation is visible instead of silent.
        const MAX_PASSES: usize = 64;
        for pass in 0..MAX_PASSES {
            let batch = self.select_batch();
            if batch.is_empty() {
                // Drain late-arrival requests: a producer that hit the
                // `probing` flag after our last non-empty selection left a
                // marker behind. Re-run one pass; the take keeps this
                // bounded even if more producers race in.
                let requested = core::mem::take(&mut *self.pump_requested.lock());
                if !requested {
                    return stats;
                }
            } else if pass + 1 == MAX_PASSES {
                let stuck = self.inner.lock().sched.deferred.clone();
                log::error!(
                    "driver-core: probe pump hit its {MAX_PASSES}-pass cap; {} device(s) still \
                     waiting: {:?}",
                    stuck.len(),
                    stuck,
                );
                return stats;
            }
            for (id, desc, driver) in batch {
                let services = self.inner.lock().services.clone();
                let (generation, core) = {
                    let inner = self.inner.lock();
                    #[expect(
                        clippy::expect_used,
                        reason = "CG-021: select_batch produced this id under the same lock; a \
                                  missing entry here is a domain-internal invariant violation, \
                                  not a driver error"
                    )]
                    let entry = inner
                        .devices
                        .get(&id)
                        .expect("batch devices are registered");
                    (entry.lifecycle.generation, entry.core.clone())
                };
                match driver.probe(&desc, crate::device::DeviceId(id), &core, services.as_ref()) {
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
        stats
    }

    /// Select the next probe batch: per pending device, the single
    /// highest-scoring unattempted driver. Equal scores resolve to the
    /// LATER registration (`max_by_key` over the registration-ordered
    /// registry returns the last maximum) — pinned by
    /// `same_score_later_registration_wins`.
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
                .filter(|driver| !attempted.contains(&driver.key()))
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
