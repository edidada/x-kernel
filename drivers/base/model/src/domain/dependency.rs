// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The deferred-probe dependency waiter: finish-phase conversion, parking
//! (with the lost-wakeup generation guard), fatal-attempt accounting, and
//! wake — including the late-provider resweep of Unclaimed devices.

use super::*;

impl DeviceDomain {
    /// Declare probe activity finished (the boot/scan phase is over).
    ///
    /// With `keep_waiting` false, devices still Deferred become `Unclaimed`
    /// because their provider has not arrived. Each is moved with a
    /// warning naming the dependency, instead of staying parked forever
    /// with no diagnostic. Devices parked on a dependency that a FUTURE
    /// driver might still publish stay deferred only if `keep_waiting` is
    /// set. This is not permanent rejection: a later dependency wake or
    /// explicit bind can retry an unclaimed device.
    pub fn finish_probe_phase(&self, keep_waiting: bool) {
        let stuck: Vec<(u64, DepSource)> = {
            let mut inner = self.inner.lock();
            if keep_waiting {
                return;
            }
            core::mem::take(&mut inner.sched.deferred)
        };
        for (id, dep) in stuck {
            let mut inner = self.inner.lock();
            if let Some(entry) = inner.devices.get_mut(&id)
                && entry.lifecycle.state == DeviceState::Deferred
            {
                entry.lifecycle.state = DeviceState::Unclaimed;
            }
            log::warn!(
                "driver-core: device {id} gave up waiting on dependency {dep} (provider never \
                 registered)"
            );
        }
    }

    /// Set `entry`'s state bookkeeping for `dep` at `generation`: the
    /// lost-wakeup guard compares this against the domain generation when a
    /// Deferred probe parks, and refreshes it on every retry or wake.
    pub(super) fn record_dep_generation(entry: &mut DeviceEntry, dep: DepSource, generation: u64) {
        match entry
            .probe
            .dep_seen_generations
            .iter_mut()
            .find(|(d, _)| *d == dep)
        {
            Some(slot) => slot.1 = generation,
            None => entry.probe.dep_seen_generations.push((dep, generation)),
        }
    }

    /// Park a Deferred probe, guarded against the lost-wakeup window: if
    /// the dependency was woken since the device last saw it, the provider
    /// registered during THIS probe's check-then-submit window — parking
    /// would sleep through an arrival that already happened. Re-attempt
    /// once per generation instead.
    pub(super) fn park_deferred(
        &self,
        inner: &mut DomainInner,
        id: u64,
        dep: DepSource,
        stats: &mut ProbeStats,
        driver_name: &str,
    ) {
        let current_gen = inner
            .sched
            .dep_generations
            .iter()
            .find(|(d, _)| *d == dep)
            .map(|(_, g)| *g)
            .unwrap_or(0);
        let seen_gen = inner
            .devices
            .get(&id)
            .and_then(|e| {
                e.probe
                    .dep_seen_generations
                    .iter()
                    .find(|(d, _)| *d == dep)
                    .map(|(_, g)| *g)
            })
            .unwrap_or(0);
        if current_gen > seen_gen {
            // The dependency moved under us: retry now.
            if let Some(entry) = inner.devices.get_mut(&id) {
                entry.lifecycle.state = DeviceState::Pending;
                Self::record_dep_generation(entry, dep, current_gen);
            }
            inner.sched.pending.push(id);
            stats.retried += 1;
            return;
        }
        if let Some(entry) = inner.devices.get_mut(&id) {
            entry.lifecycle.state = DeviceState::Deferred;
            entry.probe.last_deferred_dep = Some(dep);
            Self::record_dep_generation(entry, dep, current_gen);
        }
        inner.sched.deferred.push((id, dep));
        stats.deferred += 1;
        log::debug!("driver-core: `{driver_name}` deferred device {id} on {dep}");
    }

    /// Record a fatal probe failure: the driver joins the attempted set;
    /// the device requeues if another driver might still claim it, else
    /// turns Unclaimed.
    pub(super) fn record_failure(inner: &mut DomainInner, id: u64, driver: &Arc<dyn AnyDriver>) {
        let attempted_key = driver.key();
        let Some(entry) = inner.devices.get_mut(&id) else {
            return;
        };
        entry.binding.attempted.push(attempted_key);
        entry.lifecycle.state = DeviceState::Pending;
        inner.sched.pending.push(id);
    }

    /// Publish a dependency notification and synchronously dispatch its waiters.
    ///
    /// Returns the number queued, not the number successfully bound. Every
    /// notification advances the dependency generation, even with no waiters;
    /// a probe returning Deferred after that notification retries at commit.
    /// Call from sleepable context without locks needed by affected callbacks.
    /// Rejected management entries stay pending for a later `probe_pending`
    /// call; this method does not create a background executor.
    pub fn wake(&self, dep: DepSource) -> usize {
        let woken = {
            let mut inner = self.inner.lock();
            // Record the wake even with no waiters: the generation is what
            // a Deferred submitter checks against.
            let generation = match inner
                .sched
                .dep_generations
                .iter_mut()
                .find(|(d, _)| *d == dep)
            {
                Some((_, generation)) => {
                    *generation += 1;
                    *generation
                }
                None => {
                    inner.sched.dep_generations.push((dep, 1));
                    1
                }
            };
            let deferred = core::mem::take(&mut inner.sched.deferred);
            let mut woken = 0usize;
            for (id, waiting) in deferred {
                if waiting != dep {
                    inner.sched.deferred.push((id, waiting));
                    continue;
                }
                if let Some(entry) = inner.devices.get_mut(&id) {
                    // Only a parked (Deferred) device may be woken: a device
                    // flipped to `Removing` by a concurrent unregister_device —
                    // still sitting in the deferred list until its light
                    // path re-takes the lock — must NOT be resurrected into
                    // the probe queue (same guard as `finish_probe_phase`).
                    if entry.lifecycle.state != DeviceState::Deferred {
                        inner.sched.deferred.push((id, waiting));
                        continue;
                    }
                    entry.lifecycle.state = DeviceState::Pending;
                    Self::record_dep_generation(entry, dep, generation);
                }
                inner.sched.pending.push(id);
                woken += 1;
            }
            // Late providers: devices that deferred on this dependency,
            // exhausted the boot phase and turned Unclaimed still remember
            // what they waited for — wake them too, or nothing links a
            // post-boot provider registration to its consumers until an
            // unrelated driver registration happens to resweep.
            let unclaimed: Vec<u64> = inner
                .devices
                .iter()
                .filter(|(_, entry)| {
                    entry.lifecycle.state == DeviceState::Unclaimed
                        && entry.probe.last_deferred_dep == Some(dep)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in unclaimed {
                if let Some(entry) = inner.devices.get_mut(&id) {
                    entry.lifecycle.state = DeviceState::Pending;
                }
                inner.sched.pending.push(id);
                woken += 1;
            }
            woken
        };
        if woken > 0 {
            self.probe_pending();
        }
        woken
    }
}
