// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Domain behavior tests (see the module docs for what each engine
//! guarantees).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert_eq, def_test};

use super::*;
use crate::{
    DepKind, OfMatch, PlatformBus,
    bus::{BusInstanceId, PlatformId, platform_bus::PlatformBusData},
    desc::DeviceDescriptor,
    devres::{ClkHandle, DevIrq, IoMap, IrqAction, host_api::ClkOps},
    driver::{DriverDesc, DriverOps},
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

/// Host wake hook installed in tests (the `Host::domain().wake` analogue).
type WakeHook = Arc<dyn Fn(DepSource) + Send + Sync>;

struct MockServices {
    clk_providers: SpinNoPreempt<BTreeMap<u64, Arc<dyn ClkOps>>>,
    mapped: AtomicUsize,
    /// Test injection: fired on the clk Deferred path (mid-probe),
    /// simulating a provider registering between the dependency check
    /// and the Deferred submission (the lost-wakeup window).
    mid_check_hook: SpinNoPreempt<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Every `dev.provide` publication, in order: what the host would
    /// downcast and register.
    providers: SpinNoPreempt<alloc::vec::Vec<(DepKind, u64)>>,
    /// The host's wake hook (`Host::domain().wake`), installed by tests that
    /// exercise the provider/consumer handshake end to end.
    wake: SpinNoPreempt<Option<WakeHook>>,
}

/// The lookup trait a test provider class publishes — the base-local
/// stand-in for `kclk::ClkLookup` (driver-base must not name a host trait).
trait TestClkLookup: Send + Sync {
    fn ops(&self) -> Arc<dyn ClkOps>;
}

/// One test provider class: what a real family descriptor (e.g.
/// `driver_host::ClkProvider`) supplies for its own lookup trait.
struct TestClkClass;

impl crate::ProviderClass for TestClkClass {
    type Key = u64;
    type Lookup = dyn TestClkLookup;

    const KIND: DepKind = DepKind::Clk;
}

impl MockServices {
    /// Install the mid-check wake hook (see the struct field docs).
    fn mid_check_hook(&self, hook: &SpinNoPreempt<Option<Arc<dyn Fn() + Send + Sync>>>) {
        *self.mid_check_hook.lock() = hook.lock().clone();
    }

    fn mid_check_hook_clear(&self) {
        *self.mid_check_hook.lock() = None;
    }

    fn set_wake(&self, wake: WakeHook) {
        *self.wake.lock() = Some(wake);
    }

    fn provider_keys(&self) -> alloc::vec::Vec<(DepKind, u64)> {
        self.providers.lock().clone()
    }

    fn new() -> Arc<Self> {
        Arc::new(Self {
            mid_check_hook: SpinNoPreempt::new(None),
            clk_providers: SpinNoPreempt::new(BTreeMap::new()),
            mapped: AtomicUsize::new(0),
            providers: SpinNoPreempt::new(alloc::vec::Vec::new()),
            wake: SpinNoPreempt::new(None),
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
        _: crate::device::DeviceId,
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
        _: crate::device::DeviceId,
        spec: IrqSpec,
        _: &'static str,
        _: crate::devres::IrqHandlerFn,
    ) -> Result<DevIrq, RuntimeError> {
        Ok(DevIrq::new(
            match spec {
                IrqSpec::ResolvedVirq { virq, .. } => virq,
                // The mock does not map routes; a placeholder number
                // suffices for the tests that exercise this service.
                IrqSpec::FirmwareRoute { hwirq, .. } => crate::Virq::new(hwirq),
            },
            None,
        ))
    }

    fn alloc_coherent(
        &self,
        _: crate::device::DeviceId,
        dma: crate::DmaInfo,
        _: usize,
        _: usize,
    ) -> Result<crate::devres::CoherentMem, RuntimeError> {
        // Test mock: fake identity allocator honoring the device's DMA
        // view through the same translate() the real host uses, so the
        // mask/offset contract is exercised in tests too.
        let paddr = 0x4000_0000u64 + self.mapped.load(Ordering::Relaxed) as u64;
        match dma.translate(paddr) {
            Some(bus) => Ok(crate::devres::CoherentMem::new(
                paddr as usize,
                bus,
                0x1000,
                None,
            )),
            None => Err(RuntimeError::NoMemory),
        }
    }

    fn map_irq(
        &self,
        _: crate::device::DeviceId,
        spec: crate::resource::IrqSpec,
    ) -> Result<crate::resource::Virq, RuntimeError> {
        // Test mock: identity resolution — firmware routes "map" to
        // their hwirq (the identity domain), resolved virqs pass
        // through.
        Ok(match spec {
            crate::resource::IrqSpec::ResolvedVirq { virq, .. } => virq,
            crate::resource::IrqSpec::FirmwareRoute { hwirq, .. } => {
                crate::resource::Virq::new(hwirq)
            }
        })
    }
}

impl ClockConsumerServices for MockServices {
    fn clk_get(
        &self,
        _: crate::device::DeviceId,
        spec: ClkSpec,
    ) -> Result<ClkHandle, ServiceError> {
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

impl crate::ProviderServices for MockServices {
    fn register_provider(
        &self,
        registration: crate::ProviderRegistration,
    ) -> Result<(), RuntimeError> {
        let (kind, key) = (registration.kind(), registration.key());
        if kind != DepKind::Clk {
            return Err(RuntimeError::Unsupported);
        }
        let lookup = registration
            .lookup::<TestClkClass>()
            .ok_or(RuntimeError::Invalid)?;
        self.clk_providers.lock().insert(key, lookup.ops());
        self.providers.lock().push((kind, key));
        // The host's half of the handshake: wake exactly the consumers
        // parked on this key.
        let wake = self.wake.lock().clone();
        if let Some(wake) = wake {
            wake(DepSource::new(kind, key));
        }
        Ok(())
    }
}

impl crate::ExtensionServices for MockServices {
    fn extensions(&self) -> &crate::ExtensionRegistry {
        crate::extension::empty_registry()
    }
}

impl DeviceServices for MockServices {}

#[expect(
    dead_code,
    reason = "CG-024-lint-exceptions: unittest fixture guards exist for their Drop side effects"
)]
struct SerData {
    regs: IoMap,
    irq: DevIrq,
    clk: ClkHandle,
}

static REMOVE_RAN: AtomicBool = AtomicBool::new(false);
static PROBES: AtomicUsize = AtomicUsize::new(0);

struct TestSer;

static IDS: &[crate::PlatformId] = &[crate::PlatformId::Of(OfMatch::compatible("test,serial"))];

impl DriverOps<PlatformBus> for TestSer {
    type Data = SerData;

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<SerData, ProbeError> {
        PROBES.fetch_add(1, Ordering::Relaxed);
        let regs = dev.res().iomap(0)?;
        let irq = dev.res().request_irq_at(0, || IrqAction::Handled)?;
        let clk = dev.res().clk("baudclk")?;
        clk.prepare_enable()?;
        Ok(SerData { regs, irq, clk })
    }

    fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut SerData) {
        REMOVE_RAN.store(true, Ordering::Release);
    }
}

struct PickyDriver;

static PICKY_IDS: &[crate::PlatformId] =
    &[crate::PlatformId::Of(OfMatch::compatible("test,serial"))];

impl DriverOps<PlatformBus> for PickyDriver {
    type Data = ();

    fn probe(&self, _dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
        Err(ProbeError::Fatal(RuntimeError::Io))
    }
}

fn test_resources() -> crate::resource::ResourceMap {
    let mut resources = crate::resource::ResourceMap::new();
    resources.push_kind(crate::resource::ResourceKind::Mmio(MmioRegion::new(
        0x1000, 0x100,
    )));
    resources.push_kind(crate::resource::ResourceKind::Irq(IrqSpec::resolved(
        crate::Virq::new(32),
        crate::resource::IrqTrigger::LevelHigh,
    )));
    resources.push_named(
        "baudclk",
        crate::resource::ResourceKind::Clk(ClkSpec::by_phandle_index(0x42, 0)),
    );
    resources
}

fn test_descriptor() -> DeviceDescriptor {
    test_descriptor_for_node(1)
}

/// A consumer descriptor on a distinct firmware node: identity is per node,
/// so several test devices never share a path or dedupe accidentally.
fn test_descriptor_for_node(node_id: u32) -> DeviceDescriptor {
    let resources = test_resources();
    DeviceDescriptor::new(
        PlatformBus::ID,
        alloc::boxed::Box::new(PlatformBusData::firmware_node(node_id, &["test,serial"])),
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
fn deferred_probe_wakes_on_dependency_and_clk_class_publications() {
    PROBES.store(0, Ordering::Relaxed);
    let (domain, services) = fresh_domain();
    domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));

    let id = domain.add_device(test_descriptor()).expect("add device");
    assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Deferred);
    assert_eq!(PROBES.load(Ordering::Relaxed), 1);

    // A non-Clk dependency is exact: it must not wake the device.
    assert_eq!(domain.wake(DepSource::new(DepKind::Other, 0x99)), 0);
    assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Deferred);
    assert_eq!(PROBES.load(Ordering::Relaxed), 1);

    // Clk providers resolve inputs through the global PNAME space (Linux CCF
    // resolves branch parents by name), so a Clk publication retries every
    // Clk waiter: this re-probe cannot resolve yet and parks again.
    assert_eq!(domain.wake(DepSource::clk(0x99)), 1);
    assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Deferred);
    assert_eq!(PROBES.load(Ordering::Relaxed), 2);

    services.publish_clk(0x42);
    assert_eq!(domain.wake(DepSource::clk(0x42)), 1);
    assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Active);
    assert_eq!(PROBES.load(Ordering::Relaxed), 3);
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
    desc.data = alloc::boxed::Box::new(PlatformBusData::firmware_node(1, &["nobody,matches"]));
    let id = domain.add_device(desc).expect("add device");
    assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Unclaimed);

    // A driver that does not match must not disturb the terminal state.
    domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));
    assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Unclaimed);
}

/// Tie-break fixture: both drivers match `test,serial` with the SAME
/// score (Exact), so selection is decided by registration order alone.
struct TieA;
struct TieB;

static TIE_IDS: &[crate::PlatformId] = &[crate::PlatformId::Of(OfMatch::compatible("test,serial"))];

impl DriverOps<PlatformBus> for TieA {
    type Data = ();

    fn probe(&self, _dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
        Ok(())
    }
}

impl DriverOps<PlatformBus> for TieB {
    type Data = ();

    fn probe(&self, _dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
        Ok(())
    }
}

/// Equal MatchScore on every row: `max_by_key` returns the LAST maximum
/// over the registration-ordered registry, so the LATER registration
/// binds. This pins an explicit decision — switching to earliest-wins
/// must edit this test first.
#[def_test(serial)]
fn same_score_later_registration_wins() {
    let (domain, _services) = fresh_domain();
    domain.register_driver(DriverDesc::new("tie-a", TIE_IDS, TieA));
    domain.register_driver(DriverDesc::new("tie-b", TIE_IDS, TieB));

    let id = domain.add_device(test_descriptor()).expect("add device");
    assert_eq!(domain.snapshot(id).unwrap().driver, Some("tie-b"));
}

/// One driver definition, one registration (Linux `driver_register`
/// returns -EBUSY for the same case): a second key over the same probe
/// body would hand every device a second attempt after one Fatal
/// failure, because the twin key is not in the device's `attempted` set.
#[def_test(serial)]
fn duplicate_registration_is_rejected() {
    PROBES.store(0, Ordering::Relaxed);
    let (domain, services) = fresh_domain();
    services.publish_clk(0x42);
    assert!(domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer)));
    assert!(!domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer)));

    let id = domain.add_device(test_descriptor()).expect("add device");
    assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Active);
    // Exactly one probe body ran for the device.
    assert_eq!(PROBES.load(Ordering::Relaxed), 1);
}

/// The Fatal verdict belongs to the driver DEFINITION. Re-registering
/// the same definition must not resurrect the device's matching
/// candidates under a second key.
#[def_test(serial)]
fn duplicate_registration_cannot_retry_a_fatal_device() {
    let (domain, _services) = fresh_domain();
    assert!(domain.register_driver(DriverDesc::new("picky", PICKY_IDS, PickyDriver)));
    let id = domain.add_device(test_descriptor()).expect("add device");
    assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Unclaimed);

    assert!(!domain.register_driver(DriverDesc::new("picky", PICKY_IDS, PickyDriver)));
    assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Unclaimed);
}

/// Topology: with_parent links entries, removal cascades
/// children-first (the child's teardown completes before the parent's
/// begins), and the parent's children list stays accurate after.
#[def_test(serial)]
fn remove_cascades_children_first() {
    #[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to Clippy"
)]
#[allow(
    unknown_lints,
    unregistered_static_spinlock,
    reason = "CG-001-static-spinlock-registration: test-only lock; not tracked in lock_stat"
)]
static ORDER: SpinNoPreempt<Vec<&'static str>> = SpinNoPreempt::new(Vec::new());
    struct ChildDriver;
    struct ParentDriver;
    impl DriverOps<PlatformBus> for ChildDriver {
        type Data = ();

        fn probe(&self, _dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
            Ok(())
        }

        fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut ()) {
            ORDER.lock().push("child");
        }
    }
    impl DriverOps<PlatformBus> for ParentDriver {
        type Data = ();

        fn probe(&self, _dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
            Ok(())
        }

        fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut ()) {
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
        alloc::boxed::Box::new(PlatformBusData::firmware_node(1, &["topo,parent"])),
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
        alloc::boxed::Box::new(PlatformBusData::firmware_node(2, &["topo,child"])),
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
    #[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to Clippy"
)]
#[allow(
    unknown_lints,
    unregistered_static_spinlock,
    reason = "CG-001-static-spinlock-registration: test-only lock; not tracked in lock_stat"
)]
static CHILD_REMOVE_SAW_BUSY: SpinNoPreempt<bool> = SpinNoPreempt::new(false);
    struct ChildDriver {
        domain: &'static DeviceDomain,
        parent: u64,
    }
    impl DriverOps<PlatformBus> for ChildDriver {
        type Data = ();

        fn probe(&self, _dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
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

        fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut ()) {
            // The parent's teardown is IN PROGRESS (its cascade is
            // removing us): a concurrent remove_device(parent) must
            // see Busy — the exclusive-claim semantics — and NOT join
            // the teardown.
            let err = self
                .domain
                .remove_device(DeviceId(self.parent))
                .expect_err("concurrent parent remove must be Busy");
            assert!(matches!(err, RuntimeError::Busy));
            *CHILD_REMOVE_SAW_BUSY.lock() = true;
        }
    }
    struct ParentDriver;
    impl DriverOps<PlatformBus> for ParentDriver {
        type Data = ();

        fn probe(&self, _dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
            Ok(())
        }

        fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut ()) {}
    }
    static CHILD_IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("gate,child"))];
    static PARENT_IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("gate,parent"))];

    fn gated_descriptor(compatible: &str, firmware: u32) -> DeviceDescriptor {
        DeviceDescriptor::new(
            PlatformBus::ID,
            alloc::boxed::Box::new(PlatformBusData::firmware_node(firmware, &[compatible])),
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
    // The child's remove hook verified the in-progress exclusion
    // (recorded in the static slot — the ops instance the domain
    // holds is a by-value copy).
    assert!(*CHILD_REMOVE_SAW_BUSY.lock());
    assert_eq!(
        domain.snapshot(parent_id).unwrap().state,
        DeviceState::Removed
    );
    assert_eq!(
        domain.snapshot(child_id).unwrap().state,
        DeviceState::Removed
    );
}

/// Bus instances: a descriptor's `bus_instance` must exist and its
/// TYPE must agree; members are queryable (`bus->devices`); removing
/// the BRIDGE device ejects the instance's members (Linux
/// bridge-eject semantics).
#[def_test(serial)]
fn bus_instances_validate_query_and_bridge_eject() {
    fn pci_function(_vendor: u16, firmware: u32) -> DeviceDescriptor {
        DeviceDescriptor::new(
            crate::PlatformBus::ID, // 类型故意用 platform（测类型一致性）
            alloc::boxed::Box::new(PlatformBusData::firmware_node(firmware, &["bus,test"])),
        )
    }

    let (domain, services) = fresh_domain();
    domain.set_services(services);

    let root = domain
        .add_bus_instance(crate::PlatformBus::ID, None, "test-0")
        .expect("create root instance");

    // Identity surface: type + no bridge + label.
    let (bus, bridge, label) = domain.bus_instance_identity(root).expect("identity");
    assert_eq!(bus, crate::PlatformBus::ID);
    assert_eq!(bridge, None);
    assert_eq!(label, "test-0");

    // Type agreement: a bus_instance of the wrong type is rejected.
    // (pci_function deliberately carries PlatformBus::ID here; a
    // PciBus-typed descriptor against this instance would mismatch
    // symmetrically.)
    let member = domain
        .add_device(pci_function(0x1af4, 1).with_bus_instance(root))
        .expect("typed member attaches");

    // Membership query.
    assert_eq!(domain.bus_instance_devices(root), alloc::vec![member]);

    // Missing instance is rejected.
    let err = domain
        .add_device(pci_function(0x1af4, 2).with_bus_instance(BusInstanceId::from_raw(0xdead)))
        .expect_err("missing instance");
    assert!(matches!(err, AddDeviceError::BusInstanceMissing(0xdead)));

    // Bridge eject: anchor an instance on a bridge device; removing
    // the bridge cascades the instance's members even without device
    // parent links.
    let bridge_dev = domain
        .add_device(pci_function(0x1af4, 3))
        .expect("add bridge");
    let secondary = domain
        .add_bus_instance(crate::PlatformBus::ID, Some(bridge_dev), "test-1")
        .expect("secondary instance");
    let behind = domain
        .add_device(pci_function(0x1af4, 4).with_bus_instance(secondary))
        .expect("member behind bridge");
    assert_eq!(domain.bus_instance_devices(secondary), alloc::vec![behind]);

    domain.remove_device(bridge_dev).expect("bridge eject");
    // The member never bound — the light path FREES its entry, so the
    // snapshot is gone entirely (equivalently: fully removed).
    assert!(
        domain.snapshot(behind).is_none(),
        "bridge removal must eject the secondary instance's members"
    );
    // The member is gone from the instance's listing.
    assert!(domain.bus_instance_devices(secondary).is_empty());
    let _ = member;
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

/// `Device::resolve_irq` resolves a firmware route through the
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
    #[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to Clippy"
)]
#[allow(
    unknown_lints,
    unregistered_static_spinlock,
    reason = "CG-001-static-spinlock-registration: test-only lock; not tracked in lock_stat"
)]
static RESOLVED: SpinNoPreempt<Option<crate::Virq>> = SpinNoPreempt::new(None);
    struct SlotDriver;
    impl DriverOps<PlatformBus> for SlotDriver {
        type Data = ();

        fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
            *RESOLVED.lock() = Some(dev.res().resolve_irq(0)?);
            Ok(())
        }

        fn remove(&self, _dev: &mut Device<'_, PlatformBus>, _data: &mut ()) {}
    }
    domain.register_driver(DriverDesc::new("resolver-slot", IDS, SlotDriver));
    domain.add_device(desc).expect("add device");
    let _ = domain.probe_pending();
    // The mock's identity domain: the GIC route's hwirq IS the virq.
    assert_eq!(*RESOLVED.lock(), Some(crate::Virq::new(77)));
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
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: Dylint lint is unknown to Clippy"
    )]
    #[allow(
        unknown_lints,
        unregistered_static_spinlock,
        reason = "CG-001-static-spinlock-registration: test-only lock; not tracked in lock_stat"
    )]
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
    desc.data = alloc::boxed::Box::new(PlatformBusData::firmware_node(1, &["nobody,matches"]));
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
    desc.data = alloc::boxed::Box::new(PlatformBusData::firmware_node(1, &["nobody,matches"]));
    let id = domain.add_device(desc).expect("add device");
    assert_eq!(domain.snapshot(id).unwrap().state, DeviceState::Unclaimed);
}

#[def_test]
fn dma_view_translate_applies_offset_and_enforces_mask() {
    // Identity view: pass-through.
    let identity = crate::DmaInfo::DEFAULT;
    assert_eq!(identity.translate(0x8000_1234), Some(0x8000_1234));

    // dma-ranges offset: CPU 0x8000_0000 window appears at device 0x4000_0000.
    let offset_view = crate::DmaInfo {
        mask: u64::MAX,
        offset: 0x4000_0000,
        coherent: true,
    };
    assert_eq!(offset_view.translate(0x8000_1234), Some(0x4000_1234));

    // A 32-bit device: anything above the mask is REJECTED, not truncated —
    // the failure a real host converts into a failed allocation.
    let narrow = crate::DmaInfo {
        mask: 0xFFFF_FFFF,
        offset: 0,
        coherent: true,
    };
    assert_eq!(narrow.translate(0x1_0000_0000), None);
    assert_eq!(narrow.translate(0xFFFF_FFFF), Some(0xFFFF_FFFF));

    // The translated address (not the physical one) is what the mask gates.
    let narrow_offset = crate::DmaInfo {
        mask: 0xFFFF_FFFF,
        offset: -0x4000_0000, // device ids sit ABOVE the CPU window
        coherent: true,
    };
    assert_eq!(narrow_offset.translate(0x4000_0000), Some(0x8000_0000));
    assert_eq!(narrow_offset.translate(0x1_4000_0000), None);
}

#[def_test]
fn probe_reads_firmware_property_snapshot() {
    use crate::{device::Device, firmware::FirmwareNodeInfo};

    let (domain, services) = fresh_domain();
    let firmware = FirmwareNodeInfo {
        phandle: None,
        clock_frequency: Some(24_000_000),
        clock_cells: None,
        clock_output_name: None,
        // Raw payloads, exactly what the firmware parser emits; the view
        // types them on read.
        properties: alloc::vec![
            ("vendor,quirk-cap", &[0, 0, 0, 7][..]),
            ("vendor,label", &b"rev-b\0"[..]),
            ("dma-coherent", &[][..]),
            (
                "vendor,dma-window",
                &[0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3][..]
            ),
        ],
    };
    let data = PlatformBusData::firmware_node(1, &["test,serial"]);
    let desc = DeviceDescriptor::new(PlatformBus::ID, alloc::boxed::Box::new(data))
        .with_resources(test_resources())
        .with_firmware(Some(firmware));
    let _ = domain;

    let ctx = Device::<PlatformBus>::new(
        &desc,
        desc.data
            .as_any()
            .downcast_ref::<PlatformBusData>()
            .unwrap(),
        crate::device::DeviceId(1),
        None,
        &*services,
        crate::device::DeviceCore::synthetic(
            crate::device::DeviceId(1),
            "test-driver",
            alloc::string::String::from("platform/test"),
        ),
    );
    assert_eq!(ctx.fw().u32("vendor,quirk-cap"), Some(7));
    assert_eq!(ctx.fw().string("vendor,label"), Some("rev-b"));
    assert!(ctx.fw().flag("dma-coherent"));
    let fw = ctx.fw();
    assert_eq!(fw.prop("nope"), None);
    // Multi-cell payloads reinterpret on demand — the parse never guessed.
    assert_eq!(
        fw.prop("vendor,dma-window").and_then(|p| p.u32_list()),
        Some(alloc::vec![1, 2, 3])
    );
    // The extracted clock fast-fact is a typed FIELD, not a bag lookup:
    // `clock-frequency` bytes can end in 0x00, which the device-tree
    // disambiguation rule (used by `prop`) reads as a string payload. Linux
    // has the same ambiguity; named readers are the way to ask.
    assert_eq!(
        fw.node().and_then(|node| node.clock_frequency),
        Some(24_000_000)
    );
}

#[def_test]
fn alloc_coherent_translates_through_the_devices_dma_view() {
    // The mock's fake allocator hands out identity CPU-physical pages
    // around 0x4000_0000; the DEVICE-visible address must come back
    // translated through the descriptor's dma view.
    use crate::device::Device;

    let (_domain, services) = fresh_domain();
    let mut desc = test_descriptor();
    desc.dma = crate::DmaInfo {
        mask: 0xFFFF_FFFF,
        offset: 0x2000_0000,
        coherent: false,
    };

    let data = desc
        .data
        .as_any()
        .downcast_ref::<PlatformBusData>()
        .unwrap();
    let ctx = Device::<PlatformBus>::new(
        &desc,
        data,
        crate::device::DeviceId(1),
        None,
        &*services,
        crate::device::DeviceCore::synthetic(
            crate::device::DeviceId(1),
            "test-driver",
            alloc::string::String::from("platform/test"),
        ),
    );
    let mem = ctx.res().alloc_coherent(0x1000, 0x1000).expect("alloc");
    // paddr 0x4000_0000 → device 0x2000_0000, both under the 32-bit mask.
    assert_eq!(mem.bus_addr(), 0x2000_0000);

    // A device whose window cannot address the mock's pages fails CLEANLY.
    desc.dma = crate::DmaInfo {
        mask: 0x0FFF_FFFF,
        offset: 0,
        coherent: true,
    };
    let data = desc
        .data
        .as_any()
        .downcast_ref::<PlatformBusData>()
        .unwrap();
    let ctx = Device::<PlatformBus>::new(
        &desc,
        data,
        crate::device::DeviceId(2),
        None,
        &*services,
        crate::device::DeviceCore::synthetic(
            crate::device::DeviceId(1),
            "test-driver",
            alloc::string::String::from("platform/test"),
        ),
    );
    assert!(ctx.res().alloc_coherent(0x1000, 0x1000).is_err());
}

// ---------------------------------------------------------------------------
// The provider door: `dev.env().provide::<C>` is the single publication path, and
// publishing wakes exactly the consumers parked on that class+key.
// ---------------------------------------------------------------------------

struct TestClkProvider {
    rate: u64,
}

impl TestClkLookup for TestClkProvider {
    fn ops(&self) -> Arc<dyn ClkOps> {
        Arc::new(FixedClk { rate: self.rate })
    }
}

static TEST_CLK_PROVIDER: TestClkProvider = TestClkProvider { rate: 24_000_000 };

/// A clock-controller driver: its probe ends by publishing the provider.
struct TestCcu;

static CCU_IDS: &[crate::PlatformId] = &[crate::PlatformId::Of(OfMatch::compatible("test,ccu"))];

impl DriverOps<PlatformBus> for TestCcu {
    type Data = ();

    fn probe(&self, dev: &mut Device<'_, PlatformBus>) -> Result<(), ProbeError> {
        dev.env().provide::<TestClkClass>(0x42, &TEST_CLK_PROVIDER)
    }
}

fn ccu_descriptor() -> DeviceDescriptor {
    DeviceDescriptor::new(
        PlatformBus::ID,
        alloc::boxed::Box::new(PlatformBusData::firmware_node(2, &["test,ccu"])),
    )
}

#[def_test(serial)]
fn provider_registration_wakes_exactly_its_consumer() {
    PROBES.store(0, Ordering::Relaxed);
    let domain: &'static DeviceDomain = Box::leak(Box::new(DeviceDomain::new()));
    let services = MockServices::new();
    services.set_wake(Arc::new(move |dep| {
        domain.wake(dep);
    }));
    domain.set_services(services.clone());
    domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));
    domain.register_driver(DriverDesc::new("test-ccu", CCU_IDS, TestCcu));

    // Consumer first: the clock provider has not published yet.
    let consumer = domain.add_device(test_descriptor()).expect("add consumer");
    assert_eq!(
        domain.snapshot(consumer).unwrap().state,
        DeviceState::Deferred
    );
    assert_eq!(PROBES.load(Ordering::Relaxed), 1);
    assert!(services.provider_keys().is_empty());

    // Provider probes and publishes: the door records the class+key and the
    // host wake hook re-probes exactly the consumer parked on it.
    let provider = domain.add_device(ccu_descriptor()).expect("add provider");
    assert_eq!(
        domain.snapshot(provider).unwrap().state,
        DeviceState::Active
    );
    assert_eq!(services.provider_keys(), alloc::vec![(DepKind::Clk, 0x42)]);
    assert_eq!(
        domain.snapshot(consumer).unwrap().state,
        DeviceState::Active
    );
    assert_eq!(PROBES.load(Ordering::Relaxed), 2);

    let rate = domain.with_data::<SerData, u64>(consumer, |data| data.clk.get_rate());
    assert_eq!(rate, Some(24_000_000));
}

/// One driver instance serves many device instances (driver 1:N): the
/// registration is one descriptor, `probe` runs once per device, and each
/// binding owns its own `Data` — the driver object itself is shared and
/// carries no per-device state.
#[def_test(serial)]
fn one_driver_binds_many_devices() {
    let (domain, services) = fresh_domain();
    services.publish_clk(0x42);
    domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));
    let probes_before = PROBES.load(Ordering::Relaxed);

    let first = domain
        .add_device(test_descriptor_for_node(1))
        .expect("add first");
    let second = domain
        .add_device(test_descriptor_for_node(3))
        .expect("add second");

    for id in [first, second] {
        let snapshot = domain.snapshot(id).unwrap();
        assert_eq!(snapshot.state, DeviceState::Active);
        assert_eq!(snapshot.driver, Some("test-serial"));
        assert_eq!(
            domain.with_data::<SerData, u64>(id, |data| data.clk.get_rate()),
            Some(24_000_000)
        );
    }
    assert_eq!(PROBES.load(Ordering::Relaxed) - probes_before, 2);

    // Tearing one device down leaves the other binding intact.
    domain.remove_device(first).expect("remove first");
    assert_eq!(domain.snapshot(first).unwrap().state, DeviceState::Removed);
    assert_eq!(domain.snapshot(second).unwrap().state, DeviceState::Active);
}

/// One provider key wakes every consumer parked on it (provider 1:N
/// consumers): the wake is keyed by `DepSource`, not by device id, so two
/// devices deferring on the same phandle both resume on one publication.
#[def_test(serial)]
fn provider_publication_wakes_every_consumer_on_the_key() {
    PROBES.store(0, Ordering::Relaxed);
    let domain: &'static DeviceDomain = Box::leak(Box::new(DeviceDomain::new()));
    let services = MockServices::new();
    services.set_wake(Arc::new(move |dep| {
        domain.wake(dep);
    }));
    domain.set_services(services.clone());
    domain.register_driver(DriverDesc::new("test-serial", IDS, TestSer));
    domain.register_driver(DriverDesc::new("test-ccu", CCU_IDS, TestCcu));

    let first = domain
        .add_device(test_descriptor_for_node(1))
        .expect("add first");
    let second = domain
        .add_device(test_descriptor_for_node(3))
        .expect("add second");
    assert_eq!(domain.snapshot(first).unwrap().state, DeviceState::Deferred);
    assert_eq!(
        domain.snapshot(second).unwrap().state,
        DeviceState::Deferred
    );
    assert_eq!(services.provider_keys(), alloc::vec![]);

    let provider = domain.add_device(ccu_descriptor()).expect("add provider");
    assert_eq!(
        domain.snapshot(provider).unwrap().state,
        DeviceState::Active
    );
    assert_eq!(domain.snapshot(first).unwrap().state, DeviceState::Active);
    assert_eq!(domain.snapshot(second).unwrap().state, DeviceState::Active);
    // Two deferred first attempts plus one re-probe per woken consumer.
    assert_eq!(PROBES.load(Ordering::Relaxed), 4);
}
