// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The device domain — the instance-able owner of the driver model.
//!
//! One [`DeviceDomain`] holds the registries, the probe engine, the
//! dependency waiter, and the lifecycle dispatcher. The kernel composition
//! root creates one global domain ([`global_domain`]); tests create their
//! own — no process-global registries live in this crate.
//!
//! # Architecture (one lock, four engines)
//!
//! All state lives under ONE `SpinNoPreempt`: the atomicity of the
//! check-then-act protocols (add-time parent validation, batch selection,
//! exclusive removal claims) is a domain invariant, not an accident. The
//! engines are module boundaries — each owns its state domain and logic,
//! none owns a lock:
//!
//! * [`registry`] — device/driver/bus-instance registries: entry types,
//!   the per-entry state domains, validation, snapshots, topology links.
//! * [`probe`] — the probe engine: batch selection, out-of-lock driver
//!   execution, generation-guarded commits.
//! * [`dependency`] — the deferred-probe waiter: parking, wake (with the
//!   late-provider resweep), the lost-wakeup generation guard.
//! * [`lifecycle`] — removal orchestration: exclusive claims, the
//!   children-first cascade, observer dispatch, teardown phases.
//!
//! [`DeviceDomain`] is the thin facade owning the lock and sequencing the
//! engines; cross-engine ordering (probe ↔ remove ↔ wake) lives in
//! exactly one place.
//!
//! # Probe algorithm
//!
//! `probe_pending` selects, per descriptor, the highest-scoring registered
//! driver that has not permanently failed it, runs the probe **outside**
//! the domain lock, and commits the result:
//!
//! * `Ok(data)` — device becomes `Active`; the boxed `Data` is owned by
//!   the domain until removal.
//! * `Err(Deferred(dep))` — the device parks in `deferred` until exactly
//!   [`wake`](DeviceDomain::wake) fires with the same dependency. No
//!   polling, no retry loop.
//! * `Err(Fatal)` — the driver joins the device's `attempted` set and the
//!   next candidate is tried in the same pass; exhaustion means
//!   `Unclaimed`.

mod dependency;
mod lifecycle;
mod probe;
mod registry;

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};
use core::any::Any;

use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use crate::{
    bus::{Bus, BusDeviceData, BusId, MatchScore},
    desc::DeviceDescriptor,
    device::{BoundDevice, DeviceCtx, DeviceId},
    error::{DepSource, ProbeError, RuntimeError},
    resource::{ClkSpec, IrqSpec, MmioRegion},
    services::{ClockConsumerServices, DeviceResourceServices, DeviceServices, ServiceError},
};

/// Erased driver-private data (DriverOps::Data boxed by the framework).
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
    /// The stated bus instance does not exist (never created, or its
    /// bridge was removed first).
    BusInstanceMissing(u64),
    /// The device's bus type disagrees with the instance's.
    BusInstanceTypeMismatch {
        instance: u64,
        instance_bus: &'static str,
        device_bus: &'static str,
    },
}

/// Domain-owned lifecycle observer. Replaces the single-slot remove
/// notifier: multiple subsystems subscribe, and the callbacks are part of
/// the domain's own lifecycle (installed once, live as long as the domain).
///
/// Contract: invoked outside the domain lock, after `DriverOps::remove`
/// returned and BEFORE `Data` drops. Handlers must not re-enter the domain
/// (no probe/remove/snapshot from inside an observer).
pub trait DeviceLifecycleObserver: Send + Sync {
    /// Teardown is STARTING: the entry is `Removing`, the binding is
    /// intact, `DriverOps::remove` runs next. Observers that need the
    /// device still fully assembled see it here.
    fn device_removing(&self, owner: u64) {
        let _ = owner;
    }

    /// Teardown COMPLETED: `remove()` ran, `Data` dropped (guards
    /// released), the entry is `Removed`. The registry no longer lists
    /// the device.
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
    /// One-line device identity for `/proc` style listings (the bus's own
    /// vocabulary, via [`BusDeviceData::display_identity`]).
    pub identity: Option<alloc::string::String>,
}

trait AnyDriver: Send + Sync {
    fn name(&self) -> &'static str;
    /// The bus this driver is registered for — the cheap rejection before
    /// any vocabulary work.
    #[expect(
        dead_code,
        reason = "reserved: pre-filter before the vtable call once hot"
    )]
    fn bus_id(&self) -> BusId;
    fn matches(&self, data: &dyn BusDeviceData) -> MatchScore;
    fn probe(
        &self,
        desc: &DeviceDescriptor,
        id: DeviceId,
        services: &dyn DeviceServices,
    ) -> Result<Box<DataBox>, ProbeError>;
    fn remove(
        &self,
        desc: &DeviceDescriptor,
        id: DeviceId,
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

    fn bus_id(&self) -> BusId {
        B::ID
    }

    fn matches(&self, data: &dyn BusDeviceData) -> MatchScore {
        // Bus identity first (Linux separates bus_type matching from
        // structural checks): a driver registered for one bus never sees
        // another bus's devices.
        if data.bus_id() != B::ID {
            return MatchScore::NoMatch;
        }
        // The one downcast in the match path, guarded by the bus_id check
        // above: recover this driver's bus vocabulary and let the bus's
        // own rule score its table.
        data.as_any()
            .downcast_ref::<B::DeviceData>()
            .map(|typed| {
                self.desc
                    .ids
                    .iter()
                    .map(|entry| B::match_device(typed, entry))
                    .max()
                    .unwrap_or(MatchScore::NoMatch)
            })
            .unwrap_or(MatchScore::NoMatch)
    }

    fn probe(
        &self,
        desc: &DeviceDescriptor,
        id: DeviceId,
        services: &dyn DeviceServices,
    ) -> Result<Box<DataBox>, ProbeError> {
        // MODALIAS is DEVICE-derived (of_modalias semantics: the first
        // compatible string), not id-row-derived — module loading matches
        // the same string the bus published.
        let modalias = desc.data.modalias();
        // The bus_id check happened in matches(); probe runs on a device
        // this driver bound, so the downcast is total. A mismatching bus
        // is a domain-internal invariant violation, not a driver error.
        #[expect(
            clippy::expect_used,
            reason = "CG-021: matches() gated on bus_id before probe is scheduled — a mismatch is                       a domain-internal invariant violation, not a driver error"
        )]
        let typed = desc
            .data
            .as_any()
            .downcast_ref::<B::DeviceData>()
            .expect("the domain only probes devices the driver matched");
        let mut ctx = DeviceCtx::with_modalias(
            typed,
            id,
            &desc.resources,
            desc.dma,
            modalias.as_deref(),
            services,
        );
        let data = self.desc.ops.probe(&mut ctx)?;
        Ok(Box::new(data))
    }

    fn remove(
        &self,
        desc: &DeviceDescriptor,
        id: DeviceId,
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
        #[expect(
            clippy::expect_used,
            reason = "CG-021: the binding was created by this driver's probe — the domain never                       hands foreign data to it"
        )]
        let typed = desc
            .data
            .as_any()
            .downcast_ref::<B::DeviceData>()
            .expect("the domain only removes devices the driver bound");
        let mut bound = BoundDevice::new(typed, id, data, services);
        self.desc.ops.remove(&mut bound);
    }
}

fn driver_key(driver: &Arc<dyn AnyDriver>) -> usize {
    Arc::as_ptr(driver) as *const u8 as usize
}

/// One concrete bus instance (see [`BusInstanceId`](crate::BusInstanceId)): created by a bus
/// controller/bridge driver (or the composition root for a root bus),
/// anchoring the devices discovered on it. Removing the BRIDGE device
/// cascades the instance's member devices — the Linux bridge-eject
/// semantics (`pci_stop_and_remove_bus_device` walking the secondary
/// bus).
struct BusInstanceEntry {
    /// The bus TYPE this instance carries (a device's descriptor must
    /// agree).
    bus: BusId,
    /// The bridge/controller device that created this bus, if any (a
    /// root bus has none). Removal of this device tears the instance's
    /// members down with it.
    bridge: Option<u64>,
    /// Addressing label ("0000:00", "i2c-3") — the `bus->name`/number
    /// surface transcribed drivers print and match on.
    label: alloc::string::String,
}

struct DeviceEntry {
    /// Identity: the immutable descriptor (bus type, instance, parent,
    /// resources, firmware identity).
    desc: DeviceDescriptor,
    /// Lifecycle: state machine + teardown bookkeeping.
    lifecycle: LifecycleState,
    /// Binding: what driver claimed this device and the Data it produced.
    binding: BindingState,
    /// Topology: the devices whose `desc.parent` names this one.
    topology: TopologyState,
    /// Probe scheduling: deferred-dependency bookkeeping (parked-on-what,
    /// wake generations consumed).
    probe: ProbeState,
}

/// Lifecycle state domain of [`DeviceEntry`].
struct LifecycleState {
    state: DeviceState,
    /// Bumped on every teardown — the stale-probe-commit guard (see
    /// `probe_pending_locked`).
    generation: u64,
    /// A cascade ABORTED mid-teardown: the entry is `Removing` and the
    /// NEXT `remove_device` may CLAIM the resume — exactly one caller
    /// flips this back to false under the domain lock; concurrent
    /// removers see `Busy` (single-owner semantics, Linux
    /// `begin_removing`'s exclusive-claim equivalent).
    remove_retryable: bool,
}

/// Binding state domain of [`DeviceEntry`].
struct BindingState {
    driver: Option<Arc<dyn AnyDriver>>,
    data: Option<Box<DataBox>>,
    /// Drivers that failed this device permanently (ptr keys).
    attempted: Vec<usize>,
}

/// Topology state domain of [`DeviceEntry`].
struct TopologyState {
    children: Vec<u64>,
}

/// Probe-scheduling state domain of [`DeviceEntry`].
struct ProbeState {
    /// The dependency this device last deferred on. Kept through the
    /// `finish_probe_phase` → Unclaimed transition so a LATE provider can
    /// still wake it.
    last_deferred_dep: Option<DepSource>,
    /// Wake generations this device has already consumed per dependency —
    /// the lost-wakeup guard's bookkeeping.
    dep_seen_generations: Vec<(DepSource, u64)>,
}

struct NoServices;

impl DeviceResourceServices for NoServices {
    fn iomap(
        &self,
        _: crate::device::DeviceId,
        _: MmioRegion,
        _: &'static str,
    ) -> Result<crate::handle::IoMap, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }

    fn request_irq(
        &self,
        _: crate::device::DeviceId,
        _: IrqSpec,
        _: &'static str,
        _: crate::handle::IrqHandlerFn,
    ) -> Result<crate::handle::DevIrq, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }

    fn alloc_coherent(
        &self,
        _: crate::device::DeviceId,
        _: crate::DmaInfo,
        _: usize,
        _: usize,
    ) -> Result<crate::handle::CoherentMem, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }

    fn map_irq(
        &self,
        _: crate::device::DeviceId,
        _: crate::resource::IrqSpec,
    ) -> Result<usize, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }
}

impl ClockConsumerServices for NoServices {
    fn clk_get(
        &self,
        _: crate::device::DeviceId,
        _: ClkSpec,
    ) -> Result<crate::handle::ClkHandle, ServiceError> {
        Err(ServiceError::Fatal(RuntimeError::Unsupported))
    }
}

impl DeviceServices for NoServices {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct DomainInner {
    /// Concrete bus instances (`struct pci_bus`-shaped: bridge device +
    /// addressing label), keyed by globally-unique id.
    bus_instances: BTreeMap<u64, BusInstanceEntry>,
    /// Domain-local allocation cursor into the GLOBAL device-id space (see
    /// DEVICE_ID_ALLOCATOR): domains never hand out colliding ids, so
    /// owner-keyed global registries (class publications, devnode) stay
    /// unambiguous with multiple live domains.
    next_device_id: u64,
    services_installed: bool,
    drivers: Vec<Arc<dyn AnyDriver>>,
    devices: BTreeMap<u64, DeviceEntry>,
    /// The probe-scheduling protocol's state (the DependencyWaiter data
    /// domain): one coherent unit — the queues and their generation
    /// guard move together through park/wake/finish_phase.
    sched: SchedulingState,
    services: Arc<dyn DeviceServices>,
    observers: Vec<Arc<dyn DeviceLifecycleObserver>>,
}

/// Probe-scheduling state: the pending/deferred queues plus the wake
/// generation guard that closes the check-then-park lost-wakeup window.
struct SchedulingState {
    pending: Vec<u64>,
    deferred: Vec<(u64, DepSource)>,
    /// Wake generation per dependency: incremented on EVERY wake, even
    /// when nothing was parked. A Deferred submitted for a dependency
    /// whose generation moved since the device last deferred re-attempts
    /// instead of parking.
    dep_generations: Vec<(DepSource, u64)>,
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
        state: entry.lifecycle.state,
        driver: entry.binding.driver.as_ref().map(|driver| driver.name()),
        identity: Some(entry.desc.data.display_identity()),
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

/// The kernel's composition-root domain handle.
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
#[path = "tests.rs"]
mod tests;
