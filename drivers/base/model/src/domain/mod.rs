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
//! # Architecture (short domain lock, per-device management locks)
//!
//! Registry and queue updates share a `SpinNoPreempt`. Each device has a host
//! management lock that serializes its probe, unbind and unregister callbacks.
//! The short domain lock never spans these callbacks or Data destruction.
//!
//! * [`registry`] — device/driver/bus-instance registries: entry types,
//!   the per-entry state domains, validation, snapshots, topology links.
//! * [`probe`] — the probe engine: batch selection, out-of-lock driver
//!   execution, generation-guarded commits.
//! * [`dependency`] — the deferred-probe waiter: parking, wake (with the
//!   late-provider resweep), the lost-wakeup generation guard.
//! * [`lifecycle`] — per-device unbind and unregister; downstream ownership
//!   remains with the bus or controller.
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
    DeviceDescriptor,
    bus::{Bus, BusDeviceData, BusId},
    device::DeviceAccess,
    error::{DepSource, ProbeError, RuntimeError},
    resource::{ClkSpec, IrqSpec, MmioRegion},
    services::{ClockConsumerServices, DeviceResourceServices, DeviceServices, ServiceError},
};

/// Domain-managed registration key for a device.
///
/// Domains allocate keys from a crate-wide counter, shared across domain
/// instances. This is a runtime association key, not a hardware identifier
/// or an owning reference: holding it neither keeps a device alive nor
/// guarantees that the device remains registered, bound or online.
///
/// Kept inside the model for indexing and diagnostics; public callers use
/// device object references instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct DeviceId(pub(crate) u64);

impl DeviceId {
    /// Numeric key for diagnostics or explicitly typed record associations.
    /// The value is not a device reference or a persistent hardware identifier.
    pub(crate) const fn raw(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

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
    /// Reserved terminal state; unregistered entries are deleted from the domain.
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
    /// The host could not prepare or enter management before publication.
    Management(RuntimeError),
    /// The referenced parent object is not registered in this domain.
    /// This includes foreign-domain references and already deleted entries.
    ParentMissing(u64),
    /// The parent exists but is tearing down (`Removing`) or is already
    /// removed — it cannot accept children.
    ParentUnavailable { parent: u64, state: &'static str },
    /// The stated bus instance does not exist in this domain.
    BusInstanceMissing(u64),
    /// The instance's controller is being removed or no longer exists.
    BusInstanceUnavailable(u64),
    /// The device's bus type disagrees with the instance's.
    BusInstanceTypeMismatch {
        instance: u64,
        instance_bus: &'static str,
        device_bus: &'static str,
    },
}

/// Summary of one `probe_pending` pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeStats {
    /// Management entry was rejected; the device remains queued unchanged.
    pub rejected: usize,
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
    /// Identity retained independently of the captured lifecycle state.
    pub device: crate::DeviceRef,
    pub bus: BusId,
    pub state: DeviceState,
    pub driver: Option<&'static str>,
    /// One-line device identity for `/proc` style listings (the bus's own
    /// vocabulary, via [`BusDeviceData::display_identity`]).
    pub identity: Option<alloc::string::String>,
}

/// Domain-unique driver registration key: monotonic, never reused.
///
/// The identity behind `BindingState::attempted`. Keys are handed out at
/// registration and stay valid as NAMES even if a registration is someday
/// withdrawn: identity is a token, not the object's address — an address
/// is only a lifetime-scoped borrow of it, and reused addresses (ABA)
/// would silently alias a failed registration onto its successor. The
/// fatal verdict belongs to the DEFINITION, and identity does not depend
/// on drivers being immortal. Domain-local: attempted sets and the
/// registry and device registration keys are domain-internal bookkeeping;
/// public device identity is represented by object references.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DriverKey(u64);

trait AnyDriver: Send + Sync {
    fn name(&self) -> &'static str;
    /// The bus this driver is registered for — the cheap rejection
    /// before any vocabulary work, and half the duplicate-registration
    /// check (bus type + name).
    fn bus_id(&self) -> BusId;
    /// The registration key the domain handed out for this driver.
    fn key(&self) -> DriverKey;
    fn matches(&self, bus: BusId, data: &dyn BusDeviceData) -> bool;
    fn probe(
        &self,
        core: &alloc::sync::Arc<crate::device::DeviceCore>,
        services: &dyn DeviceServices,
    ) -> Result<Box<DataBox>, ProbeError>;
    fn remove(
        &self,
        core: &alloc::sync::Arc<crate::device::DeviceCore>,
        data: &mut DataBox,
        services: &dyn DeviceServices,
    );
}

struct RegisteredDriver<B: Bus, P: crate::driver::DriverOps<B>> {
    identity: Arc<crate::driver::DriverIdentity>,
    ids: &'static [B::IdEntry],
    ops: P,
    key: DriverKey,
}

impl<B: Bus, P: crate::driver::DriverOps<B>> AnyDriver for RegisteredDriver<B, P> {
    fn name(&self) -> &'static str {
        self.identity.name
    }

    fn bus_id(&self) -> BusId {
        B::ID
    }

    fn key(&self) -> DriverKey {
        self.key
    }

    fn matches(&self, bus: BusId, data: &dyn BusDeviceData) -> bool {
        // Bus identity first (Linux separates bus_type matching from
        // structural checks): a driver registered for one bus never sees
        // another bus's devices.
        if bus != B::ID || data.bus_id() != B::ID {
            return false;
        }
        // The one downcast in the match path, guarded by the bus_id check
        // above: recover this driver's bus vocabulary and let the bus's
        // own rule match its table.
        data.as_any()
            .downcast_ref::<B::DeviceData>()
            .is_some_and(|typed| self.ids.iter().any(|entry| B::match_device(typed, entry)))
    }

    fn probe(
        &self,
        core: &alloc::sync::Arc<crate::device::DeviceCore>,
        services: &dyn DeviceServices,
    ) -> Result<Box<DataBox>, ProbeError> {
        // The claim is known when probing STARTS — record the driver on
        // the live identity card before any publication can occur.
        core.bind_driver(self.identity.clone());
        // MODALIAS is DEVICE-derived (of_modalias semantics: the first
        // compatible string), not id-row-derived — module loading matches
        // the same string the bus published.
        let reference = crate::DeviceRef { core: core.clone() };
        let desc = reference.descriptor();
        let modalias = desc.bus_data.modalias();
        // The bus_id check happened in matches(); probe runs on a device
        // this driver bound, so the downcast is total. A mismatching bus
        // is a domain-internal invariant violation, not a driver error.
        #[expect(
            clippy::expect_used,
            reason = "CG-021: matches() gated on bus_id before probe is scheduled — a mismatch is \
                      a domain-internal invariant violation, not a driver error"
        )]
        let mut ctx = DeviceAccess::new(&reference, &self.identity, modalias.as_deref(), services)
            .expect("the domain only probes devices the driver matched");
        let data = self
            .ops
            .probe(&mut ctx, &crate::Environment::new(services))?;
        Ok(Box::new(data))
    }

    fn remove(
        &self,
        core: &alloc::sync::Arc<crate::device::DeviceCore>,
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
        let reference = crate::DeviceRef { core: core.clone() };
        #[expect(
            clippy::expect_used,
            reason = "CG-021: the binding was created by this driver's probe — the domain never                       hands foreign data to it"
        )]
        let mut dev = DeviceAccess::new(&reference, &self.identity, None, services)
            .expect("the domain only removes devices the driver bound");
        self.ops
            .remove(&mut dev, &crate::Environment::new(services), data);
    }
}

/// One concrete bus instance (see [`BusInstanceId`](crate::BusInstanceId)): created by a bus
/// controller/bridge driver (or the composition root for a root bus),
/// anchoring the devices discovered on it. The bus controller owns member
/// teardown; the model does not infer recursive ownership from topology.
struct BusInstanceEntry {
    /// The authoritative bus type for this instance's devices.
    bus: BusId,
    /// The bridge/controller device that created this bus, if any (a
    /// root bus has none). The controller must arrange member teardown.
    /// Retained for instance introspection after device teardown; this reference
    /// owns no driver binding or hardware resource.
    bridge: Option<crate::DeviceRef>,
    /// Addressing label ("0000:00", "i2c-3") — the `bus->name`/number
    /// surface transcribed drivers print and match on.
    label: alloc::string::String,
}

struct DeviceEntry {
    /// Host chosen at registration, including the management-lock factory.
    services: Arc<dyn DeviceServices>,
    /// Shared identity and immutable descriptor, retained by device references
    /// and class publications independently of this entry's binding.
    core: alloc::sync::Arc<crate::device::DeviceCore>,
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
    /// Bumped on teardown; successful probe results are checked before commit.
    generation: u64,
}

/// Binding state domain of [`DeviceEntry`].
struct BindingState {
    driver: Option<Arc<dyn AnyDriver>>,
    data: Option<Box<DataBox>>,
    /// Driver registrations that failed this device permanently.
    ///
    /// Keys are never reused ([`DriverKey`] is monotonic), so entries
    /// stay harmless even if a registration is someday withdrawn —
    /// identity does not depend on drivers being immortal.
    attempted: Vec<DriverKey>,
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
        _: crate::DeviceContext<'_>,
        _: MmioRegion,
        _: &'static str,
    ) -> Result<crate::IoMap, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }

    fn request_irq(
        &self,
        _: crate::DeviceContext<'_>,
        _: IrqSpec,
        _: &'static str,
        _: crate::IrqHandlerFn,
    ) -> Result<crate::DevIrq, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }

    fn alloc_coherent(
        &self,
        _: crate::DeviceContext<'_>,
        _: usize,
        _: usize,
    ) -> Result<crate::CoherentMem, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }

    fn map_irq(
        &self,
        _: crate::DeviceContext<'_>,
        _: crate::resource::IrqSpec,
    ) -> Result<crate::resource::Virq, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }
}

impl ClockConsumerServices for NoServices {
    fn clk_get(
        &self,
        _: crate::DeviceContext<'_>,
        _: ClkSpec,
    ) -> Result<crate::ClkHandle, ServiceError> {
        Err(ServiceError::Fatal(RuntimeError::Unsupported))
    }
}

impl crate::ExtensionServices for NoServices {
    fn extensions(&self) -> &crate::ExtensionRegistry {
        crate::extension::empty_registry()
    }
}

impl crate::ProviderServices for NoServices {
    fn register_provider(
        &self,
        _registration: crate::ProviderRegistration,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::Unsupported)
    }
}

impl DeviceServices for NoServices {}

struct DomainInner {
    /// Concrete bus instances (`struct pci_bus`-shaped: bridge device +
    /// addressing label), keyed by globally-unique id.
    bus_instances: BTreeMap<u64, BusInstanceEntry>,
    /// Reserved next device key from the crate-wide allocator.
    /// Management keys remain distinct across domain instances.
    next_device_id: u64,
    drivers: Vec<Arc<dyn AnyDriver>>,
    /// Allocation cursor for [`DriverKey`]s (domain-local: keys are
    /// only ever compared against this domain's own registries).
    next_driver_key: u64,
    devices: BTreeMap<u64, DeviceEntry>,
    /// The probe-scheduling protocol's state (the DependencyWaiter data
    /// domain): one coherent unit — the queues and their generation
    /// guard move together through park/wake/finish_phase.
    sched: SchedulingState,
    services: Arc<dyn DeviceServices>,
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

/// Crate-wide allocation source for device keys and bus-instance identifiers.
/// Domain creation reserves a device key; device insertion reserves the next.
/// Numeric allocation does not grant ownership, liveness or cross-domain access.
static DEVICE_ID_ALLOCATOR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[cfg(unittest)]
type DispatchHookFn = dyn Fn(&DeviceDomain, bool) + Send + Sync;

pub struct DeviceDomain {
    identity: Arc<()>,
    inner: SpinNoPreempt<DomainInner>,
    #[cfg(unittest)]
    dispatch_hook_fn: SpinNoPreempt<Option<Arc<DispatchHookFn>>>,
}

#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: insertion validates instance membership; \
              instance records remain for removed-device snapshots"
)]
fn bus_type(instances: &BTreeMap<u64, BusInstanceEntry>, desc: &DeviceDescriptor) -> BusId {
    instances
        .get(&desc.bus_instance.raw())
        .expect("registered device retains its bus instance")
        .bus
}

/// Build a consistent snapshot: one filling for `snapshot()` and
/// `devices()` alike (they used to disagree — identity was dead in
/// `snapshot()`). The identity reference remains live metadata, while state
/// and driver capture the domain's view at the time of the snapshot.
fn snapshot_of(entry: &DeviceEntry, bus: BusId) -> DeviceSnapshot {
    DeviceSnapshot {
        device: crate::DeviceRef {
            core: entry.core.clone(),
        },
        bus,
        state: entry.lifecycle.state,
        driver: entry.binding.driver.as_ref().map(|driver| driver.name()),
        identity: None,
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

unittest::path_mod_test!(tests, "../../ut-tests/domain.rs");
