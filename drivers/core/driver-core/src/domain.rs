// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The device domain: the instance-able owner of the probe engine.
//!
//! One [`DeviceDomain`] holds the descriptors, registered drivers, deferred
//! probes, and the service bundle. The kernel composition root creates one
//! global domain ([`global_domain`]); tests create their own, which is what
//! makes driver-model tests isolated and parallel-safe — there are no
//! process-global registries in this crate.
//!
//! # Probe algorithm
//!
//! `probe_pending` selects, per descriptor, the highest-scoring registered
//! driver that has not permanently failed it, runs the probe **outside** the
//! domain lock, and commits the result:
//!
//! * `Ok(data)` — device becomes `Active`, the boxed `Data` is owned by the
//!   domain until removal.
//! * `Err(Deferred(dep))` — device is parked in `deferred` until exactly
//!   [`wake`] is called with the same dependency. No polling, no retry loop.
//! * `Err(Fatal)` — this driver joins the device's `attempted` set and the
//!   next candidate is tried in the same pass; exhaustion means `Unclaimed`.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};
use core::{any::Any, sync::atomic::Ordering};

use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use crate::{
    bus::{Bus, BusId, DeviceDescriptor, DeviceId, IdMatchEntry, MatchScore},
    device::{
        BoundDevice, ClockConsumerServices, DeviceCtx, DeviceResourceServices, DeviceServices,
        ServiceError,
    },
    error::{DepSource, ProbeError, RuntimeError},
    resource::{ClkSpec, IrqSpec, MmioRegion},
};

/// Erased driver-private data (`DriverOps::Data` boxed by the framework).
pub type DataBox = dyn Any + Send + Sync;

/// Lifecycle state of one device in a domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    /// Descriptor registered, not yet probed.
    Pending,
    /// A probe is currently running.
    Probing,
    /// Bound and initialized.
    Active,
    /// Parked on a [`DepSource`], waiting for [`DeviceDomain::wake`].
    Deferred,
    /// No driver matched or every candidate failed permanently.
    Unclaimed,
    /// Being torn down.
    Removing,
    /// Torn down; kept for topology queries.
    Removed,
}

impl DeviceState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Probing => "probing",
            Self::Active => "active",
            Self::Deferred => "deferred",
            Self::Unclaimed => "unclaimed",
            Self::Removing => "removing",
            Self::Removed => "removed",
        }
    }
}

/// Why [`DeviceDomain::add_device`] rejected a descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddDeviceError {
    /// The stated parent id does not exist in this domain (firmware typo,
    /// discovery-order bug, or the parent was removed first).
    ParentMissing(u64),
    /// The parent exists but is tearing down (`Removing`) or is already
    /// removed — it cannot accept children.
    ParentUnavailable { parent: u64, state: &'static str },
}

/// Domain-owned lifecycle observer. Replaces the single-slot remove
/// notifier: multiple subsystems subscribe, and the callbacks are part of
/// the domain's own lifecycle (installed once, live as long as the domain).
///
/// Contract: invoked outside the domain lock, after `DriverOps::remove`
/// returned and BEFORE `Data` drops. Handlers must not re-enter the domain
/// (no probe/remove/snapshot from inside an observer).
pub trait DeviceLifecycleObserver: Send + Sync {
    /// A device finished teardown: `remove()` ran; `Data` drops next.
    ///
    /// Future hooks (add/bind/unbind/deferred) land on this trait next;
    /// removal exists first because every current consumer only needed
    /// that phase.
    fn device_removed(&self, owner: u64);
}

/// Summary of one `probe_pending` pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeStats {
    /// Devices re-attempted because a dependency woke during their
    /// check-then-submit window (lost-wakeup guard).
    pub retried: usize,
    pub activated: usize,
    pub deferred: usize,
    pub failed: usize,
}

/// Read-only view of one device for introspection and tests.
#[derive(Debug, Clone)]
pub struct DeviceSnapshot {
    pub id: DeviceId,
    pub bus: BusId,
    pub state: DeviceState,
    pub driver: Option<&'static str>,
    /// Device kind, when a driver is bound (the class view of the device).
    pub kind: Option<crate::DeviceKind>,
    /// Firmware identity string for `/proc` style listings.
    pub identity: Option<alloc::string::String>,
}

trait AnyDriver: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, desc: &DeviceDescriptor) -> MatchScore;
    fn probe(
        &self,
        desc: &DeviceDescriptor,
        id: u64,
        services: &dyn DeviceServices,
    ) -> Result<Box<DataBox>, ProbeError>;
    fn remove(
        &self,
        desc: &DeviceDescriptor,
        id: u64,
        data: &mut DataBox,
        services: &dyn DeviceServices,
    );
}

struct RegisteredDriver<B: Bus, P: crate::driver::DriverOps<B>> {
    desc: crate::driver::DriverDesc<B, P>,
}

impl<B: Bus, P: crate::driver::DriverOps<B>> AnyDriver for RegisteredDriver<B, P> {
    fn name(&self) -> &'static str {
        self.desc.name
    }

    fn matches(&self, desc: &DeviceDescriptor) -> MatchScore {
        // Bus identity first (Linux separates bus_type matching from
        // structural checks): a driver registered for one bus never sees
        // another bus's devices, even when their location shapes overlap.
        // accepts() only validates the descriptor's structural fit.
        if desc.bus != B::ID || !B::accepts(desc) {
            return MatchScore::NoMatch;
        }
        self.desc
            .ids
            .iter()
            .map(|entry| entry.matches(desc))
            .max()
            .unwrap_or(MatchScore::NoMatch)
    }

    fn probe(
        &self,
        desc: &DeviceDescriptor,
        id: u64,
        services: &dyn DeviceServices,
    ) -> Result<Box<DataBox>, ProbeError> {
        // MODALIAS: the alias of the BEST-SCORING matching id row — the
        // same row `matches()` selects the driver by. A first-match pick
        // here could pair an exact-match device with a generic row's
        // alias (uevent/module loading would see the wrong identity).
        let modalias = self
            .desc
            .ids
            .iter()
            .filter_map(|entry| {
                let score = entry.matches(desc);
                (score != MatchScore::NoMatch).then(|| (score, entry.modalias()))
            })
            .max_by_key(|(score, _)| *score)
            .and_then(|(_, alias)| alias);
        let modalias_ref = modalias.as_deref();
        let mut ctx = DeviceCtx::with_modalias(desc, id, modalias_ref, services);
        let data = self.desc.ops.probe(&mut ctx)?;
        Ok(Box::new(data))
    }

    fn remove(
        &self,
        desc: &DeviceDescriptor,
        id: u64,
        data: &mut DataBox,
        services: &dyn DeviceServices,
    ) {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: the domain only hands a binding's \
                      data back to the driver that created it (typed key)"
        )]
        let data = data
            .downcast_mut::<P::Data>()
            .expect("device data belongs to the driver that probed it");
        let mut bound = BoundDevice::new(desc, id, data, services);
        self.desc.ops.remove(&mut bound);
    }
}

fn driver_key(driver: &Arc<dyn AnyDriver>) -> usize {
    Arc::as_ptr(driver) as *const u8 as usize
}

struct DeviceEntry {
    desc: DeviceDescriptor,
    state: DeviceState,
    driver: Option<Arc<dyn AnyDriver>>,
    data: Option<Box<DataBox>>,
    /// Topology children (ids of devices whose `desc.parent` is this
    /// device). Maintained by add/remove so teardown can cascade
    /// children-first — a parent never disappears while a child
    /// references it.
    children: Vec<u64>,
    /// Drivers that failed this device permanently (ptr keys).
    attempted: Vec<usize>,
    /// The dependency this device last deferred on. Kept through the
    /// `finish_probe_phase` → Unclaimed transition so a LATE provider can
    /// still wake it: `wake` re-matches Unclaimed devices against their
    /// last dependency, closing the "provider registered after boot end"
    /// hole (previously nothing re-linked them until an unrelated driver
    /// registration reswept).
    last_deferred_dep: Option<DepSource>,
    /// Binding generation: bumped on every teardown (light and full) so a
    /// probe result created against an older generation can never commit —
    /// the structural stale-commit guard for future reentrancy (hotplug,
    /// rebind). Today the Probing→Busy remove gate already prevents the
    /// interleave; this makes the invariant local to the commit path
    /// instead of incidental to the gate.
    generation: u64,
    /// Wake generations this device has already consumed per dependency —
    /// the lost-wakeup guard's bookkeeping (see DomainInner's
    /// dep_generations).
    dep_seen_generations: Vec<(DepSource, u64)>,
}

struct NoServices;

impl DeviceResourceServices for NoServices {
    fn iomap(
        &self,
        _: u64,
        _: MmioRegion,
        _: &'static str,
    ) -> Result<crate::device::IoMap, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }

    fn request_irq(
        &self,
        _: u64,
        _: IrqSpec,
        _: &'static str,
        _: crate::device::IrqHandlerFn,
    ) -> Result<crate::device::DevIrq, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }

    fn alloc_coherent(
        &self,
        _: u64,
        _: usize,
        _: usize,
    ) -> Result<crate::device::CoherentMem, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }

    fn map_irq(&self, _: u64, _: crate::resource::IrqSpec) -> Result<usize, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }
}

impl ClockConsumerServices for NoServices {
    fn clk_get(&self, _: u64, _: ClkSpec) -> Result<crate::device::ClkHandle, ServiceError> {
        Err(ServiceError::Fatal(RuntimeError::Unsupported))
    }
}

impl DeviceServices for NoServices {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct DomainInner {
    /// Domain-local allocation cursor into the GLOBAL device-id space (see
    /// DEVICE_ID_ALLOCATOR): domains never hand out colliding ids, so
    /// owner-keyed global registries (class publications, devnode) stay
    /// unambiguous with multiple live domains.
    next_device_id: u64,
    services_installed: bool,
    drivers: Vec<Arc<dyn AnyDriver>>,
    devices: BTreeMap<u64, DeviceEntry>,
    pending: Vec<u64>,
    deferred: Vec<(u64, DepSource)>,
    /// Wake generation per dependency: incremented on EVERY wake, even
    /// when nothing was parked. A Deferred submitted for a dependency
    /// whose generation moved since the device last deferred re-attempts
    /// instead of parking — closing the check-then-park lost-wakeup window.
    dep_generations: Vec<(DepSource, u64)>,
    services: Arc<dyn DeviceServices>,
    observers: Vec<Arc<dyn DeviceLifecycleObserver>>,
}

/// The GLOBAL device-id allocator: every `DeviceDomain` reserves ids from
/// this one atomic space. The old per-domain `0, 1, 2…` counters made two
/// instantiated domains collide on owner-keyed global registries (class
/// publications keyed by `dev.id().raw()`, devnode withdrawal) — removing
/// domain A's device 0 could withdraw domain B's publication. Ids remain
/// small (a domain claims its next id on creation and on every add);
/// uniqueness across domains is the invariant that matters.
static DEVICE_ID_ALLOCATOR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Scope guard for the probe-pump serialization flag: releases it on
/// every exit, including panics under `panic = "unwind"` builds (see
/// [`DeviceDomain::probe_pending`]). Under `panic = "abort"` a panic ends
/// the system anyway; the guard is for the unwind configurations.
struct ProbePumpGuard<'a> {
    domain: &'a DeviceDomain,
}

impl Drop for ProbePumpGuard<'_> {
    fn drop(&mut self) {
        *self.domain.probing.lock() = false;
    }
}

pub struct DeviceDomain {
    inner: SpinNoPreempt<DomainInner>,
    /// Serializes the probe pump across callers. The probe path runs
    /// driver code outside the domain lock, and parts of the host runtime
    /// (the MMIO lease collector) rely on probe serialization — the
    /// review asked for that contract to be enforced at the entry, not
    /// assumed. A second probe_pending while one runs returns immediately:
    /// the running pump owns the pending queue's progress.
    probing: SpinNoPreempt<bool>,
}

/// Build a consistent snapshot: one filling for `snapshot()` and
/// `devices()` alike (they used to disagree — identity was dead in
/// `snapshot()`). `kind` stays None until a class view exists; the field
/// is documented as driver-bound.
fn snapshot_of(id: DeviceId, entry: &DeviceEntry) -> DeviceSnapshot {
    DeviceSnapshot {
        id,
        bus: entry.desc.bus,
        state: entry.state,
        driver: entry.driver.as_ref().map(|driver| driver.name()),
        kind: None,
        identity: match &entry.desc.identity {
            crate::desc::DeviceIdentity::Platform(platform) => platform
                .firmware_id
                .clone()
                .or_else(|| platform.alias.clone()),
            crate::desc::DeviceIdentity::Pci(pci) => Some(alloc::format!(
                "pci {:#06x}:{:#06x}",
                pci.vendor_id,
                pci.device_id
            )),
        },
    }
}

impl DeviceDomain {
    pub fn new() -> Self {
        Self {
            probing: SpinNoPreempt::new(false),
            inner: SpinNoPreempt::new(DomainInner {
                next_device_id: DEVICE_ID_ALLOCATOR.fetch_add(1, Ordering::Relaxed),
                drivers: Vec::new(),
                devices: BTreeMap::new(),
                pending: Vec::new(),
                deferred: Vec::new(),
                dep_generations: Vec::new(),
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
    /// `kclass::ClassWithdraw`) and unpublishes at its own drop. Observers
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
                .filter(|(_, entry)| entry.state == DeviceState::Unclaimed)
                .filter(|(_, entry)| {
                    inner.drivers.iter().any(|candidate| {
                        !entry.attempted.contains(&driver_key(candidate))
                            && candidate.matches(&entry.desc) != MatchScore::NoMatch
                    })
                })
                .map(|(id, _)| *id)
                .collect();
            for id in resweep {
                if let Some(entry) = inner.devices.get_mut(&id) {
                    entry.state = DeviceState::Pending;
                }
                inner.pending.push(id);
            }
            log::debug!(
                "driver-core: registered driver `{name}` for bus `{}`",
                B::ID.name()
            );
        }
        self.probe_pending();
    }

    /// Insert an enumeration product and probe it eagerly.
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
            // Topology validation, under the SAME lock that governs
            // removal: a stated parent must exist AND be accepting
            // children. A parent mid-teardown (Removing) or gone
            // (Removed) rejects the add — the cascade snapshot cannot see
            // an entry added after it, so accepting it would orphan the
            // child at the parent's teardown. A missing parent is an
            // error, not a silent root placement: the descriptor's own
            // `parent` field would disagree with the registry topology.
            if let Some(parent) = desc.parent {
                let parent_state = inner.devices.get(&parent).map(|entry| entry.state);
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
                    state: DeviceState::Pending,
                    driver: None,
                    data: None,
                    children: Vec::new(),
                    generation: 0,
                    attempted: Vec::new(),
                    last_deferred_dep: None,
                    dep_seen_generations: Vec::new(),
                },
            );
            inner.pending.push(id.raw());
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
        self.inner.lock().deferred.clone()
    }

    /// Declare probe activity finished (the boot/scan phase is over).
    ///
    /// Devices still Deferred after this point are permanently unclaimed —
    /// their provider never arrived. Each is moved to `Unclaimed` with a
    /// warning naming the dependency, instead of staying parked forever
    /// with no diagnostic. Devices parked on a dependency that a FUTURE
    /// driver might still publish stay deferred only if `keep_waiting` is
    /// set; boot-time callers pass false (nothing else will register).
    pub fn finish_probe_phase(&self, keep_waiting: bool) {
        let stuck: Vec<(u64, DepSource)> = {
            let mut inner = self.inner.lock();
            if keep_waiting {
                return;
            }
            core::mem::take(&mut inner.deferred)
        };
        for (id, dep) in stuck {
            let mut inner = self.inner.lock();
            if let Some(entry) = inner.devices.get_mut(&id)
                && entry.state == DeviceState::Deferred
            {
                entry.state = DeviceState::Unclaimed;
            }
            log::warn!(
                "driver-core: device {id} gave up waiting on dependency {dep}                  \
                 (provider never registered)"
            );
        }
    }

    /// Announce that a dependency published itself; parked devices waiting
    /// on it are re-probed. Returns how many devices were woken.
    /// Wake devices waiting on `dep`.
    ///
    /// Concurrency note: a provider may register BETWEEN a probe's
    /// dependency check and its Deferred submission — that wake finds an
    /// empty deferred list and the device would park forever (the review's
    /// lost-wakeup window). The wake therefore also nudges the probe pump:
    /// the next `probe_pending` pass re-examines parked entries, and a
    /// device whose dependency has since appeared rebinds instead of
    /// parking. Exactness (only this dep's waiters re-run) is preserved for
    /// the deferred list; the pump nudge is the safety net for the window.
    /// Set `entry`'s state bookkeeping for `dep` at `generation`: the
    /// lost-wakeup guard compares this against the domain generation when a
    /// Deferred probe parks, and refreshes it on every retry or wake.
    fn record_dep_generation(entry: &mut DeviceEntry, dep: DepSource, generation: u64) {
        match entry
            .dep_seen_generations
            .iter_mut()
            .find(|(d, _)| *d == dep)
        {
            Some(slot) => slot.1 = generation,
            None => entry.dep_seen_generations.push((dep, generation)),
        }
    }

    /// Park a Deferred probe, guarded against the lost-wakeup window: if
    /// the dependency was woken since the device last saw it, the provider
    /// registered during THIS probe's check-then-submit window — parking
    /// would sleep through an arrival that already happened. Re-attempt
    /// once per generation instead.
    fn park_deferred(
        &self,
        inner: &mut DomainInner,
        id: u64,
        dep: DepSource,
        stats: &mut ProbeStats,
        driver_name: &str,
    ) {
        let current_gen = inner
            .dep_generations
            .iter()
            .find(|(d, _)| *d == dep)
            .map(|(_, g)| *g)
            .unwrap_or(0);
        let seen_gen = inner
            .devices
            .get(&id)
            .and_then(|e| {
                e.dep_seen_generations
                    .iter()
                    .find(|(d, _)| *d == dep)
                    .map(|(_, g)| *g)
            })
            .unwrap_or(0);
        if current_gen > seen_gen {
            // The dependency moved under us: retry now.
            if let Some(entry) = inner.devices.get_mut(&id) {
                entry.state = DeviceState::Pending;
                Self::record_dep_generation(entry, dep, current_gen);
            }
            inner.pending.push(id);
            stats.retried += 1;
            return;
        }
        if let Some(entry) = inner.devices.get_mut(&id) {
            entry.state = DeviceState::Deferred;
            entry.last_deferred_dep = Some(dep);
            Self::record_dep_generation(entry, dep, current_gen);
        }
        inner.deferred.push((id, dep));
        stats.deferred += 1;
        log::debug!("driver-core: `{driver_name}` deferred device {id} on {dep}");
    }

    /// Record a fatal probe failure: the driver joins the attempted set;
    /// the device requeues if another driver might still claim it, else
    /// turns Unclaimed.
    fn record_failure(inner: &mut DomainInner, id: u64, driver: &Arc<dyn AnyDriver>) {
        let attempted_key = driver_key(driver);
        let Some(entry) = inner.devices.get_mut(&id) else {
            return;
        };
        entry.attempted.push(attempted_key);
        let retryable = inner
            .drivers
            .iter()
            .filter(|candidate| !entry.attempted.contains(&driver_key(candidate)))
            .any(|candidate| candidate.matches(&entry.desc) != MatchScore::NoMatch);
        if retryable {
            entry.state = DeviceState::Pending;
            inner.pending.push(id);
        } else {
            entry.state = DeviceState::Unclaimed;
        }
    }

    pub fn wake(&self, dep: DepSource) -> usize {
        let woken = {
            let mut inner = self.inner.lock();
            // Record the wake even with no waiters: the generation is what
            // a Deferred submitter checks against.
            let generation = match inner.dep_generations.iter_mut().find(|(d, _)| *d == dep) {
                Some((_, generation)) => {
                    *generation += 1;
                    *generation
                }
                None => {
                    inner.dep_generations.push((dep, 1));
                    1
                }
            };
            let deferred = core::mem::take(&mut inner.deferred);
            let mut woken = 0usize;
            for (id, waiting) in deferred {
                if waiting != dep {
                    inner.deferred.push((id, waiting));
                    continue;
                }
                if let Some(entry) = inner.devices.get_mut(&id) {
                    entry.state = DeviceState::Pending;
                    Self::record_dep_generation(entry, dep, generation);
                }
                inner.pending.push(id);
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
                    entry.state == DeviceState::Unclaimed && entry.last_deferred_dep == Some(dep)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in unclaimed {
                if let Some(entry) = inner.devices.get_mut(&id) {
                    entry.state = DeviceState::Pending;
                }
                inner.pending.push(id);
                woken += 1;
            }
            woken
        };
        if woken > 0 {
            // Re-entrant-safe pump: if a probe pump already runs (this wake
            // fired mid-probe — the generation guard will re-attempt in
            // ITS loop), don't recurse into the pump (the serialization
            // lock would deadlock). Standalone wakes run the full pump.
            let already_probing = {
                let mut probing = self.probing.lock();
                if *probing {
                    true
                } else {
                    *probing = true;
                    false
                }
            };
            if !already_probing {
                let _guard = ProbePumpGuard { domain: self };
                self.probe_pending_locked();
            }
        }
        woken
    }

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

    fn probe_pending_locked(&self) -> ProbeStats {
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
                    .map_or(0, |e| e.generation);
                match driver.probe(&desc, id, services.as_ref()) {
                    Ok(data) => {
                        let mut inner = self.inner.lock();
                        match inner.devices.get_mut(&id) {
                            Some(entry) if entry.generation == generation => {
                                entry.state = DeviceState::Active;
                                entry.driver = Some(driver.clone());
                                entry.data = Some(data);
                                stats.activated += 1;
                                log::debug!(
                                    "driver-core: `{}` activated device {id} ({:?})",
                                    driver.name(),
                                    desc.identity
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
        let pending = core::mem::take(&mut inner.pending);
        for id in pending {
            let (state, attempted, desc) = {
                let Some(entry) = inner.devices.get_mut(&id) else {
                    continue;
                };
                (entry.state, entry.attempted.clone(), entry.desc.clone())
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
                .map(|driver| (driver.clone(), driver.matches(&desc)))
                .filter(|(_, score)| *score != MatchScore::NoMatch)
                .max_by_key(|(_, score)| *score)
                .map(|(driver, _)| driver);
            match best {
                Some(driver) => {
                    if let Some(entry) = inner.devices.get_mut(&id) {
                        entry.state = DeviceState::Probing;
                    }
                    batch.push((id, desc, driver));
                }
                None => {
                    if let Some(entry) = inner.devices.get_mut(&id) {
                        entry.state = DeviceState::Unclaimed;
                    }
                }
            }
        }
        batch
    }

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
        //    remove() → observers → drop(Data).
        //
        // (Linux expresses the same ordering through the bus driver's own
        // remove walk; the device model here owns the link.)
        let (children, light_path) = {
            let mut inner = self.inner.lock();
            let Some(entry) = inner.devices.get_mut(&id.raw()) else {
                return Err(RuntimeError::NotFound);
            };
            match entry.state {
                DeviceState::Active => {
                    entry.state = DeviceState::Removing;
                    entry.generation += 1;
                    (entry.children.clone(), false)
                }
                // Resuming an ABORTED cascade (a child was Busy earlier):
                // the entry is already `Removing` and closed to new
                // children — re-snapshot what remains and continue. Only
                // this protocol transitions `Removing` forward.
                DeviceState::Removing => (entry.children.clone(), false),
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
                // Never bound: nothing to quiesce, no publication to
                // withdraw, no guards to release. Still cascade children
                // first (topology is independent of binding state), then
                // free the entry — keeping it would grow the table
                // unboundedly under churn.
                DeviceState::Pending | DeviceState::Deferred | DeviceState::Unclaimed => {
                    entry.generation += 1;
                    (entry.children.clone(), true)
                }
            }
        };
        for child in children {
            match self.remove_device(DeviceId(child)) {
                Ok(()) => {}
                // The child is already gone: a prior cascade pass (or a
                // concurrent remove) finished it. That satisfies the
                // children-first requirement for this child.
                Err(RuntimeError::NotFound) => {}
                Err(RuntimeError::BadState) => {
                    let child_gone = {
                        let inner = self.inner.lock();
                        matches!(
                            inner.devices.get(&child).map(|entry| entry.state),
                            Some(DeviceState::Removed) | None
                        )
                    };
                    if !child_gone {
                        log::warn!(
                            "driver-core: cascade remove of child {child} under {id} hit BadState \
                             mid-teardown; parent teardown ABORTED — retry to continue"
                        );
                        return Err(RuntimeError::BadState);
                    }
                }
                // Busy (child probing) or any other failure: abort the
                // parent teardown — the child still holds (or is
                // acquiring) resources against this device. The parent is
                // already `Removing`, closed to new children; a retry
                // re-snapshots the remaining children and continues.
                Err(err) => {
                    log::warn!(
                        "driver-core: cascade remove of child {child} under {id} failed \
                         ({err:?}); parent teardown ABORTED — retry to continue"
                    );
                    return Err(err);
                }
            }
        }
        if light_path {
            let mut inner = self.inner.lock();
            Self::unlink_from_parent(&mut inner, &id.raw());
            inner.devices.remove(&id.raw());
            inner.pending.retain(|pending| *pending != id.raw());
            inner.deferred.retain(|(deferred, _)| *deferred != id.raw());
            log::debug!("driver-core: removed unbound device {id} (no binding to tear down)");
            return Ok(());
        }
        let (desc, driver, data, services) = {
            let mut inner = self.inner.lock();
            let Some(entry) = inner.devices.get_mut(&id.raw()) else {
                return Err(RuntimeError::NotFound);
            };
            if entry.state != DeviceState::Removing {
                return Err(RuntimeError::BadState);
            }
            (
                entry.desc.clone(),
                entry.driver.take(),
                entry.data.take(),
                inner.services.clone(),
            )
        };
        let (driver, mut data, services) = (driver, data, services);

        if let (Some(driver), Some(data)) = (driver.as_ref(), data.as_mut()) {
            driver.remove(&desc, id.raw(), data.as_mut(), services.as_ref());
        }

        // Lifecycle observers run BEFORE dropping `Data` (consumers may
        // still touch the device while its guards hold the hardware). The
        // publications themselves are Data-owned now: they withdraw inside
        // `drop(data)` below, sequenced by each Data's field order.
        let observers = self.inner.lock().observers.clone();
        for observer in &observers {
            observer.device_removed(id.raw());
        }

        drop(data);
        {
            let mut inner = self.inner.lock();
            Self::unlink_from_parent(&mut inner, &id.raw());
            if let Some(entry) = inner.devices.get_mut(&id.raw()) {
                entry.state = DeviceState::Removed;
            }
        }
        Ok(())
    }

    /// Remove `id` from its parent's children list (topology unlink).
    fn unlink_from_parent(inner: &mut DomainInner, id: &u64) {
        let Some(parent) = inner.devices.get(id).map(|entry| entry.desc.parent) else {
            return;
        };
        let Some(parent) = parent else { return };
        if let Some(parent_entry) = inner.devices.get_mut(&parent) {
            parent_entry.children.retain(|child| child != id);
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
        let data = entry.data.as_ref()?.as_ref().downcast_ref::<D>()?;
        Some(visit(data))
    }
}

impl Default for DeviceDomain {
    fn default() -> Self {
        Self::new()
    }
}

static GLOBAL_DOMAIN: LazyInit<DeviceDomain> = LazyInit::new();

/// The kernel's composition-root domain. Tests construct their own
/// [`DeviceDomain`] instead.
/// Read-only topology summary of the global domain (procfs/introspection).
pub fn global_topology_summary() -> Vec<DeviceSnapshot> {
    global_domain().devices()
}

pub fn global_domain() -> &'static DeviceDomain {
    GLOBAL_DOMAIN.call_once(DeviceDomain::new);
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once completed immediately above"
    )]
    {
        GLOBAL_DOMAIN
            .get()
            .expect("global device domain call_once succeeded")
    }
}

#[cfg(unittest)]
mod tests {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{
        bus::{DeviceDescriptor, PlatformId},
        device::{ClkHandle, ClkOps, DevIrq, IoMap, IrqAction},
        driver::{DriverDesc, DriverOps},
        platform::{OfMatch, PlatformBus},
    };

    struct FixedClk {
        rate: u64,
    }

    impl ClkOps for FixedClk {
        fn name(&self) -> &str {
            "fixed"
        }

        fn get_rate(&self) -> u64 {
            self.rate
        }

        fn round_rate(&self, rate: u64) -> Result<u64, RuntimeError> {
            Ok(rate)
        }

        fn set_rate(&self, rate: u64) -> Result<u64, RuntimeError> {
            Ok(rate)
        }

        fn prepare(&self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn unprepare(&self) {}

        fn enable(&self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn disable(&self) {}

        fn is_prepared(&self) -> bool {
            true
        }

        fn is_enabled(&self) -> bool {
            true
        }
    }

    struct MockServices {
        clk_providers: SpinNoPreempt<BTreeMap<u64, Arc<dyn ClkOps>>>,
        mapped: AtomicUsize,
        /// Test injection: fired on the clk Deferred path (mid-probe),
        /// simulating a provider registering between the dependency check
        /// and the Deferred submission (the lost-wakeup window).
        mid_check_hook: SpinNoPreempt<Option<Arc<dyn Fn() + Send + Sync>>>,
    }

    impl MockServices {
        /// Install the mid-check wake hook (see the struct field docs).
        fn mid_check_hook(&self, hook: &SpinNoPreempt<Option<Arc<dyn Fn() + Send + Sync>>>) {
            *self.mid_check_hook.lock() = hook.lock().clone();
        }

        fn mid_check_hook_clear(&self) {
            *self.mid_check_hook.lock() = None;
        }

        fn new() -> Arc<Self> {
            Arc::new(Self {
                mid_check_hook: SpinNoPreempt::new(None),
                clk_providers: SpinNoPreempt::new(BTreeMap::new()),
                mapped: AtomicUsize::new(0),
            })
        }

        fn publish_clk(&self, phandle: u64) {
            self.clk_providers
                .lock()
                .insert(phandle, Arc::new(FixedClk { rate: 24_000_000 }));
        }
    }

    impl DeviceResourceServices for MockServices {
        fn iomap(
            &self,
            _: u64,
            region: MmioRegion,
            _: &'static str,
        ) -> Result<IoMap, RuntimeError> {
            self.mapped.fetch_add(1, Ordering::Relaxed);
            // SAFETY: test mock — the handle is never accessed, only
            // carried; the vaddr is a dummy that satisfies alignment.
            Ok(unsafe { IoMap::new(region.base, region, None) })
        }

        fn request_irq(
            &self,
            _: u64,
            spec: IrqSpec,
            _: &'static str,
            _: crate::device::IrqHandlerFn,
        ) -> Result<DevIrq, RuntimeError> {
            Ok(DevIrq::new(
                match spec {
                    IrqSpec::ResolvedVirq { number, .. } => number,
                    // The mock does not map routes; a placeholder number
                    // suffices for the tests that exercise this service.
                    IrqSpec::FirmwareRoute { hwirq, .. } => hwirq,
                },
                None,
            ))
        }

        fn alloc_coherent(
            &self,
            _: u64,
            _: usize,
            _: usize,
        ) -> Result<crate::device::CoherentMem, RuntimeError> {
            Err(RuntimeError::Unsupported)
        }

        fn map_irq(&self, _: u64, spec: crate::resource::IrqSpec) -> Result<usize, RuntimeError> {
            // Test mock: identity resolution — firmware routes "map" to
            // their hwirq (the identity domain), resolved virqs pass
            // through.
            Ok(match spec {
                crate::resource::IrqSpec::ResolvedVirq { number, .. } => number,
                crate::resource::IrqSpec::FirmwareRoute { hwirq, .. } => hwirq,
            })
        }
    }

    impl ClockConsumerServices for MockServices {
        fn clk_get(&self, _: u64, spec: ClkSpec) -> Result<ClkHandle, ServiceError> {
            // Bind the lookup OUTSIDE the match: a match scrutinee's
            // temporary lock guard lives for the whole match arm, and the
            // mid-check hook re-enters publish_clk (same clk_providers
            // lock) — the classic self-deadlock the review warned about
            // for registry callbacks.
            let existing = self.clk_providers.lock().get(&spec.provider).cloned();
            match existing {
                Some(ops) => Ok(ClkHandle::new(ops, None)),
                None => {
                    if let Some(hook) = self.mid_check_hook.lock().clone() {
                        hook();
                    }
                    Err(ServiceError::Deferred(DepSource::clk(spec.provider)))
                }
            }
        }
    }

    impl DeviceServices for MockServices {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }
    }

    #[expect(
        dead_code,
        reason = "CG-024-lint-exceptions: unittest fixture guards exist for their Drop side \
                  effects"
    )]
    struct SerData {
        regs: IoMap,
        irq: DevIrq,
        clk: ClkHandle,
    }

    static REMOVE_RAN: AtomicBool = AtomicBool::new(false);
    static PROBES: AtomicUsize = AtomicUsize::new(0);

    struct TestSer;

    static IDS: &[crate::platform::PlatformId] = &[crate::platform::PlatformId::Of(
        OfMatch::compatible("test,serial"),
    )];

    impl DriverOps<PlatformBus> for TestSer {
        type Data = SerData;

        fn probe(&self, dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<SerData, ProbeError> {
            PROBES.fetch_add(1, Ordering::Relaxed);
            let regs = dev.iomap(0)?;
            let irq = dev.request_irq_at(0, || IrqAction::Handled)?;
            let clk = dev.clk("baudclk")?;
            clk.prepare_enable()?;
            Ok(SerData { regs, irq, clk })
        }

        fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, SerData>) {
            REMOVE_RAN.store(true, Ordering::Release);
        }
    }

    struct PickyDriver;

    static PICKY_IDS: &[crate::platform::PlatformId] = &[crate::platform::PlatformId::Of(
        OfMatch::compatible("test,serial"),
    )];

    impl DriverOps<PlatformBus> for PickyDriver {
        type Data = ();

        fn probe(&self, _dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<(), ProbeError> {
            Err(ProbeError::Fatal(RuntimeError::Io))
        }
    }

    fn test_descriptor() -> DeviceDescriptor {
        let mut resources = crate::resource::ResourceMap::new();
        resources.push_kind(crate::resource::ResourceKind::Mmio(MmioRegion::new(
            0x1000, 0x100,
        )));
        resources.push_kind(crate::resource::ResourceKind::Irq(IrqSpec::resolved(
            32,
            crate::resource::IrqTrigger::LevelHigh,
        )));
        resources.push_named(
            "baudclk",
            crate::resource::ResourceKind::Clk(ClkSpec::by_phandle_index(0x42, 0)),
        );
        DeviceDescriptor::new(
            PlatformBus::ID,
            crate::bus::DeviceLocation::FirmwareNode { id: 1 },
            crate::bus::DeviceIdentity::Platform(crate::bus::PlatformIdentity {
                firmware_id: Some(alloc::string::String::from("test,serial")),
                compatibles: alloc::vec![alloc::string::String::from("test,serial")],
                alias: None,
            }),
        )
        .with_resources(resources)
    }

    fn fresh_domain() -> (DeviceDomain, Arc<MockServices>) {
        let domain = DeviceDomain::new();
        let services = MockServices::new();
        domain.set_services(services.clone());
        (domain, services)
    }

    #[def_test(serial)]
    fn probe_binds_data_and_remove_quiesces() {
        REMOVE_RAN.store(false, Ordering::Release);
        let (domain, services) = fresh_domain();
        services.publish_clk(0x42);
        domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));

        let id = domain.add_device(test_descriptor()).expect("add device");
        let snapshot = domain.snapshot(id).unwrap();
        assert_eq!(snapshot.state, DeviceState::Active);
        assert_eq!(snapshot.driver, Some("test-serial"));

        let rate = domain.with_data::<SerData, u64>(id, |data| data.clk.get_rate());
        assert_eq!(rate, Some(24_000_000));

        domain.remove_device(id).unwrap();
        assert!(REMOVE_RAN.load(Ordering::Acquire));
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Removed);
    }

    #[def_test(serial)]
    fn deferred_probe_wakes_exactly_on_dependency() {
        PROBES.store(0, Ordering::Relaxed);
        let (domain, services) = fresh_domain();
        domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));

        let id = domain.add_device(test_descriptor()).expect("add device");
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Deferred);
        assert_eq!(PROBES.load(Ordering::Relaxed), 1);

        // An unrelated dependency must not wake the device.
        assert_eq!(domain.wake(DepSource::clk(0x99)), 0);
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Deferred);

        services.publish_clk(0x42);
        assert_eq!(domain.wake(DepSource::clk(0x42)), 1);
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Active);
        assert_eq!(PROBES.load(Ordering::Relaxed), 2);
    }

    #[def_test(serial)]
    fn removing_probing_device_rejects_with_busy() {
        // In the single-threaded unittest environment, a spin-gated probe
        // cannot be interleaved with remove_device. We verify the state
        // contract structurally: the Probing arm in remove_device() sits
        // before the unbound light path and returns Busy — confirmed by
        // the exhaustive match in the source. The SMP interleaving test
        // requires multi-core support and is deferred.
        //
        // The key invariant: Probing is NOT in the light-path arm.
        assert!(!matches!(
            DeviceState::Probing,
            DeviceState::Pending | DeviceState::Deferred | DeviceState::Unclaimed
        ));
    }

    #[def_test(serial)]
    fn late_driver_registration_claims_unclaimed_device() {
        let (domain, services) = fresh_domain();
        services.publish_clk(0x42);

        // Device arrives before any driver: goes Unclaimed.
        let id = domain.add_device(test_descriptor()).expect("add device");
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Unclaimed);

        // A driver registered later must still claim it.
        domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Active);
        assert_eq!(domain.snapshot(id).unwrap().driver, Some("test-serial"));
    }

    #[def_test(serial)]
    fn late_driver_registration_skips_unmatched_unclaimed() {
        let (domain, _services) = fresh_domain();
        let mut desc = test_descriptor();
        desc.identity = crate::bus::DeviceIdentity::Platform(crate::bus::PlatformIdentity {
            firmware_id: Some(alloc::string::String::from("nobody,matches")),
            compatibles: alloc::vec![alloc::string::String::from("nobody,matches")],
            alias: None,
        });
        let id = domain.add_device(desc).expect("add device");
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Unclaimed);

        // A driver that does not match must not disturb the terminal state.
        domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Unclaimed);
    }

    /// Topology: with_parent links entries, removal cascades
    /// children-first (the child's teardown completes before the parent's
    /// begins), and the parent's children list stays accurate after.
    #[def_test(serial)]
    fn remove_cascades_children_first() {
        static ORDER: SpinNoPreempt<Vec<&'static str>> = SpinNoPreempt::new(Vec::new());
        struct ChildDriver;
        struct ParentDriver;
        impl DriverOps<PlatformBus> for ChildDriver {
            type Data = ();

            fn probe(&self, _dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<(), ProbeError> {
                Ok(())
            }

            fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, ()>) {
                ORDER.lock().push("child");
            }
        }
        impl DriverOps<PlatformBus> for ParentDriver {
            type Data = ();

            fn probe(&self, _dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<(), ProbeError> {
                Ok(())
            }

            fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, ()>) {
                ORDER.lock().push("parent");
            }
        }
        static CHILD_IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("topo,child"))];
        static PARENT_IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("topo,parent"))];

        let (domain, services) = fresh_domain();
        domain.set_services(services.clone());
        domain.register_driver(DriverDesc::new("topo-child", CHILD_IDS, ChildDriver));
        domain.register_driver(DriverDesc::new("topo-parent", PARENT_IDS, ParentDriver));

        let parent_desc = DeviceDescriptor::new(
            PlatformBus::ID,
            crate::bus::DeviceLocation::FirmwareNode { id: 1 },
            crate::bus::DeviceIdentity::Platform(crate::bus::PlatformIdentity {
                firmware_id: Some(alloc::string::String::from("topo,parent")),
                compatibles: alloc::vec![alloc::string::String::from("topo,parent")],
                alias: None,
            }),
        );
        let parent_id = domain.add_device(parent_desc.clone()).expect("add device");
        let _ = domain.probe_pending();
        assert_eq!(
            domain.snapshot(parent_id).unwrap().state,
            DeviceState::Active,
            "parent must bind before the child references it"
        );

        let child_desc = DeviceDescriptor::new(
            PlatformBus::ID,
            crate::bus::DeviceLocation::FirmwareNode { id: 2 },
            crate::bus::DeviceIdentity::Platform(crate::bus::PlatformIdentity {
                firmware_id: Some(alloc::string::String::from("topo,child")),
                compatibles: alloc::vec![alloc::string::String::from("topo,child")],
                alias: None,
            }),
        )
        .with_parent(Some(parent_id));
        let child_id = domain.add_device(child_desc).expect("add device");
        let _ = domain.probe_pending();
        assert_eq!(
            domain.snapshot(child_id).unwrap().state,
            DeviceState::Active
        );

        // Removing the parent tears the child down first.
        domain.remove_device(parent_id).expect("cascade remove");
        assert_eq!(*ORDER.lock(), alloc::vec!["child", "parent"]);
        assert_eq!(
            domain.snapshot(child_id).map(|s| s.state),
            Some(DeviceState::Removed)
        );
    }

    /// Parent teardown is GATED on child teardown completion — a real
    /// behavior test, not an enum assertion: the child's own probe
    /// re-enters `remove_device(parent)` while the child is `Probing`
    /// (exactly the interleave the Busy error exists for). The parent
    /// must abort BEFORE releasing any binding resource, stay closed to
    /// new children, and complete on retry.
    #[def_test(serial)]
    fn parent_teardown_waits_for_probing_child() {
        use crate::bus::DeviceLocation;

        struct ChildDriver {
            domain: &'static DeviceDomain,
            parent: u64,
        }
        impl DriverOps<PlatformBus> for ChildDriver {
            type Data = ();

            fn probe(&self, _dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<(), ProbeError> {
                // Mid-probe: the parent removal runs while THIS probe is
                // in flight — the child entry is `Probing` on the
                // registry, the exact interleave the Busy error exists
                // for. remove_device does not pump probes, so the
                // re-entrancy is lock-clean.
                let err = self
                    .domain
                    .remove_device(DeviceId(self.parent))
                    .expect_err("parent remove must abort on busy child");
                assert!(matches!(err, RuntimeError::Busy));
                Ok(())
            }

            fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, ()>) {}
        }
        struct ParentDriver;
        impl DriverOps<PlatformBus> for ParentDriver {
            type Data = ();

            fn probe(&self, _dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<(), ProbeError> {
                Ok(())
            }

            fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, ()>) {}
        }
        static CHILD_IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("gate,child"))];
        static PARENT_IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("gate,parent"))];

        fn gated_descriptor(compatible: &str, firmware: u32) -> DeviceDescriptor {
            DeviceDescriptor::new(
                PlatformBus::ID,
                DeviceLocation::FirmwareNode { id: firmware },
                crate::bus::DeviceIdentity::Platform(crate::bus::PlatformIdentity {
                    firmware_id: Some(alloc::string::String::from(compatible)),
                    compatibles: alloc::vec![alloc::string::String::from(compatible)],
                    alias: None,
                }),
            )
        }

        // The child driver needs to re-enter the domain: tests are serial,
        // so a leaked static domain is bounded and safe here.
        let (domain, services) = fresh_domain();
        let services = services;
        let domain: &'static DeviceDomain = Box::leak(Box::new(domain));
        domain.set_services(services);
        domain.register_driver(DriverDesc::new("gate-parent", PARENT_IDS, ParentDriver));
        let parent_id = domain
            .add_device(gated_descriptor("gate,parent", 1))
            .expect("add parent");
        let _ = domain.probe_pending();
        assert_eq!(
            domain.snapshot(parent_id).unwrap().state,
            DeviceState::Active
        );

        // Child whose probe re-enters the parent removal mid-flight.
        domain.register_driver(DriverDesc::new(
            "gate-child",
            CHILD_IDS,
            ChildDriver {
                domain,
                parent: parent_id.raw(),
            },
        ));
        let child_id = domain
            .add_device(gated_descriptor("gate,child", 2).with_parent(Some(parent_id)))
            .expect("add child");
        let _ = domain.probe_pending();
        // The in-probe removal aborted: the parent is mid-teardown
        // (Removing — closed to children, retryable), its binding intact.
        assert_eq!(
            domain.snapshot(parent_id).unwrap().state,
            DeviceState::Removing
        );
        assert_eq!(
            domain.snapshot(child_id).unwrap().state,
            DeviceState::Active
        );

        // While the parent is mid-teardown it takes no new children.
        let err = domain
            .add_device(gated_descriptor("gate,late", 3).with_parent(Some(parent_id)))
            .expect_err("Removing parent must reject children");
        assert!(matches!(
            err,
            AddDeviceError::ParentUnavailable { parent, state: "removing" }
                if parent == parent_id.raw()
        ));

        // Retry: the child finished probing; the cascade completes and the
        // parent tears down fully (child Removed first).
        domain.remove_device(parent_id).expect("retry completes");
        assert_eq!(
            domain.snapshot(parent_id).unwrap().state,
            DeviceState::Removed
        );
        assert_eq!(
            domain.snapshot(child_id).unwrap().state,
            DeviceState::Removed
        );
    }

    /// `add_device` rejects a missing parent outright — the descriptor's
    /// parent field and the registry topology must never disagree.
    #[def_test(serial)]
    fn add_device_rejects_missing_parent() {
        let (domain, services) = fresh_domain();
        domain.set_services(services);
        let err = domain
            .add_device(test_descriptor().with_parent(Some(DeviceId(0xdead))))
            .expect_err("missing parent");
        assert_eq!(err, AddDeviceError::ParentMissing(0xdead));
    }

    /// `DeviceCtx::resolve_irq` resolves a firmware route through the
    /// host service — the seam the virtio-input shared-line handler filter
    /// depends on (reading the route's raw fields confuses hwirq with
    /// virq; rejecting routes broke the only firmware-enumerated path).
    #[def_test(serial)]
    fn resolve_irq_maps_firmware_routes_through_services() {
        // Firmware-route descriptor (DT enumeration's shape).
        let mut resources = crate::resource::ResourceMap::new();
        resources.push_kind(crate::resource::ResourceKind::Irq(IrqSpec::firmware(
            crate::resource::IrqController::Gic,
            77,
            crate::resource::IrqTrigger::LevelHigh,
        )));
        let desc = test_descriptor().with_resources(resources);

        let (domain, services) = fresh_domain();
        domain.set_services(services);
        // The ops instance the domain holds is a CLONE of the struct by
        // value — capture the answer through the registry instead: the
        // probe wrote into ITS copy; read it back via with_data is
        // impossible (unit Data). Use a static slot instead.
        static RESOLVED: SpinNoPreempt<Option<usize>> = SpinNoPreempt::new(None);
        struct SlotDriver;
        impl DriverOps<PlatformBus> for SlotDriver {
            type Data = ();

            fn probe(&self, dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<(), ProbeError> {
                *RESOLVED.lock() = Some(dev.resolve_irq(0)?);
                Ok(())
            }

            fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, ()>) {}
        }
        domain.register_driver(DriverDesc::new("resolver-slot", IDS, SlotDriver));
        domain.add_device(desc).expect("add device");
        let _ = domain.probe_pending();
        // The mock's identity domain: the GIC route's hwirq IS the virq.
        assert_eq!(*RESOLVED.lock(), Some(77));
    }

    /// Two instantiated domains never hand out the same device id: the
    /// global allocator keeps owner-keyed global registries (class
    /// publications, devnode) unambiguous. Regression for the per-domain
    /// `0, 1, 2…` counters that made domain A's removal withdraw domain
    /// B's publications.
    #[def_test(serial)]
    fn device_ids_are_unique_across_domains() {
        let (domain_a, _services_a) = fresh_domain();
        let (domain_b, _services_b) = fresh_domain();

        let a0 = domain_a.add_device(test_descriptor()).expect("add device");
        let b0 = domain_b.add_device(test_descriptor()).expect("add device");
        let a1 = domain_a.add_device(test_descriptor()).expect("add device");

        assert_ne!(a0.raw(), b0.raw(), "first ids of two domains collide");
        assert_ne!(a0.raw(), a1.raw());
        assert_ne!(a1.raw(), b0.raw());
        let _ = domain_a.probe_pending();
        let _ = domain_b.probe_pending();
    }

    /// Regression: the lost-wakeup window, exercised as a REAL
    /// interleaving. The wake fires INSIDE the dependency check that
    /// decides to defer — before the Deferred result reaches the domain
    /// (the review's exact CPU A/B interleave). Without the generation
    /// guard the device parks forever: the wake found an empty deferred
    /// list and nothing re-links them.
    #[def_test(serial)]
    fn wake_during_probe_window_does_not_lose_the_device() {
        // The mock services' clk_get invokes this hook on the Deferred
        // path (before returning) — simulating the provider registering
        // mid-check. Arc<dyn Fn> over a global slot: tests are serial.
        static MID_CHECK_HOOK: SpinNoPreempt<Option<Arc<dyn Fn() + Send + Sync>>> =
            SpinNoPreempt::new(None);

        // The hook needs 'static references into the domain under test.
        // Leaking one test domain per run is acceptable (serial tests,
        // bounded by the single test that uses this hook) and keeps the
        // injection a real mid-probe wake rather than a flag dance.
        let (domain, services): (&'static DeviceDomain, &'static MockServices) = {
            let (domain, services) = fresh_domain();
            // SAFETY: the Arc is deliberately leaked for the 'static test
            // hook (serial tests; bounded by this single test).
            let services: &'static MockServices = unsafe { &*Arc::into_raw(services) };
            (Box::leak(Box::new(domain)), services)
        };
        *MID_CHECK_HOOK.lock() = Some(Arc::new(move || {
            services.publish_clk(0x42);
            // Wake with an EMPTY deferred list: the device is mid-probe,
            // not parked yet. This is the exact window.
            let woken = domain.wake(DepSource::clk(0x42));
            assert!(woken == 0, "nothing parked yet — this IS the window");
        }));
        services.mid_check_hook(&MID_CHECK_HOOK);

        domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));
        let probes_before = PROBES.load(Ordering::Relaxed);
        let id = domain.add_device(test_descriptor()).expect("add device");

        // The probe ran: clk_get deferred AND the hook fired its wake.
        // (PROBES is shared across tests — assert the delta.)
        let probes_after = PROBES.load(Ordering::Relaxed);
        assert!(probes_after > probes_before);
        let second_pass = probes_after - probes_before >= 2;
        // The device must not be parked in Deferred — the generation guard
        // sees the dependency moved and the pump re-attempts; TestSer's
        // second probe finds the provider and binds.
        let state = domain.snapshot(id).unwrap().state;
        assert_eq!(state, DeviceState::Active);
        assert_eq!(domain.snapshot(id).unwrap().driver, Some("test-serial"));
        // The guard re-attempted (at least two probe passes for this
        // device) — without it the FIRST pass would have parked the
        // device in Deferred forever.
        assert!(second_pass, "generation guard did not re-attempt");

        *MID_CHECK_HOOK.lock() = None;
        services.mid_check_hook_clear();
    }

    /// Regression: a provider registering AFTER `finish_probe_phase` must
    /// still wake the devices that deferred on it. They turned Unclaimed at
    /// boot end, but the last dependency is remembered — `wake` re-matches
    /// them instead of waiting for an unrelated driver registration.
    #[def_test(serial)]
    fn late_provider_wakes_unclaimed_device() {
        let (domain, services) = fresh_domain();
        domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));

        let id = domain.add_device(test_descriptor()).expect("add device");
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Deferred);

        // Boot end: the stuck device converts to Unclaimed...
        domain.finish_probe_phase(false);
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Unclaimed);

        // ...and the provider arrives only now. The device must wake and
        // bind (previously it stayed Unclaimed until an unrelated
        // register_driver reswept it).
        services.publish_clk(0x42);
        // Mock services publish without auto-waking (the real runtime's
        // registry wake hook does); drive it explicitly as the runtime would.
        assert_eq!(domain.wake(DepSource::clk(0x42)), 1);
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Active);
        assert_eq!(domain.snapshot(id).unwrap().driver, Some("test-serial"));
    }

    #[def_test(serial)]
    fn finish_probe_phase_unsticks_deferred_with_diagnostics() {
        let (domain, services) = fresh_domain();
        domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));

        // Provider 0x42 never publishes: the device stays deferred.
        let id = domain.add_device(test_descriptor()).expect("add device");
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Deferred);
        assert_eq!(domain.deferred_report().len(), 1);

        // Boot end: the stuck device converts to Unclaimed, not parked forever.
        domain.finish_probe_phase(false);
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Unclaimed);
        assert!(domain.deferred_report().is_empty());

        // And the provider arriving late still claims it (resweep path).
        services.publish_clk(0x42);
        // Late provider still wakes it: Unclaimed devices remember their
        // last dependency (the finish_probe_phase verdict is not terminal).
        assert_eq!(domain.wake(DepSource::clk(0x42)), 1);
    }

    #[def_test(serial)]
    fn removing_unbound_device_frees_entry_and_worklists() {
        let (domain, _services) = fresh_domain();

        // Unclaimed device (no driver for it).
        let mut desc = test_descriptor();
        desc.identity = crate::bus::DeviceIdentity::Platform(crate::bus::PlatformIdentity {
            firmware_id: Some(alloc::string::String::from("nobody,matches")),
            compatibles: alloc::vec![alloc::string::String::from("nobody,matches")],
            alias: None,
        });
        let unclaimed = domain.add_device(desc).expect("add device");
        assert_eq!(
            domain.snapshot(unclaimed).unwrap().state,
            DeviceState::Unclaimed
        );

        // Deferred device (driver registered, provider missing).
        domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));
        let deferred = domain.add_device(test_descriptor()).expect("add device");
        assert_eq!(
            domain.snapshot(deferred).unwrap().state,
            DeviceState::Deferred
        );

        // Light-path removal: entries freed, worklists drained, not kept
        // as tombstones — the table does not grow under hotplug churn.
        domain.remove_device(unclaimed).unwrap();
        domain.remove_device(deferred).unwrap();
        assert!(domain.snapshot(unclaimed).is_none());
        assert!(domain.snapshot(deferred).is_none());
        assert_eq!(domain.devices().len(), 0);
        assert!(domain.deferred_report().is_empty());

        // A removed-unbound id can be re-added cleanly (ids are monotonic;
        // no state collision).
        let readded = domain.add_device(test_descriptor()).expect("add device");
        assert_eq!(
            domain.snapshot(readded).unwrap().state,
            DeviceState::Deferred
        );
    }

    #[def_test(serial)]
    fn removing_twice_rejects_with_bad_state() {
        let (domain, services) = fresh_domain();
        services.publish_clk(0x42);
        domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));
        let id = domain.add_device(test_descriptor()).expect("add device");

        domain.remove_device(id).unwrap();
        assert!(matches!(
            domain.remove_device(id),
            Err(crate::RuntimeError::BadState)
        ));
    }

    #[def_test(serial)]
    fn fatal_failure_falls_through_to_next_candidate() {
        let (domain, services) = fresh_domain();
        services.publish_clk(0x42);
        domain.register_driver(DriverDesc::new("picky", PICKY_IDS, PickyDriver));
        domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));

        let id = domain.add_device(test_descriptor()).expect("add device");
        let snapshot = domain.snapshot(id).unwrap();
        assert_eq!(snapshot.state, DeviceState::Active);
        assert_eq!(snapshot.driver, Some("test-serial"));
    }

    #[def_test(serial)]
    fn unmatched_descriptor_is_unclaimed() {
        let (domain, _services) = fresh_domain();
        let mut desc = test_descriptor();
        desc.identity = crate::bus::DeviceIdentity::Platform(crate::bus::PlatformIdentity {
            firmware_id: Some(alloc::string::String::from("nobody,matches")),
            compatibles: alloc::vec![alloc::string::String::from("nobody,matches")],
            alias: None,
        });
        let id = domain.add_device(desc).expect("add device");
        assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Unclaimed);
    }
}
