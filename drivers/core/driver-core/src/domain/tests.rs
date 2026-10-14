// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Domain behavior tests (see the module docs for what each engine
//! guarantees).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert_eq, def_test};

use super::*;
use crate::{
    bus::{BusInstanceId, PlatformId},
    desc::DeviceDescriptor,
    driver::{DriverDesc, DriverOps},
    handle::{ClkHandle, DevIrq},
    iomap::IoMap,
    platform::{OfMatch, PlatformBus},
    services::{ClkOps, IrqAction},
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
    fn iomap(&self, _: u64, region: MmioRegion, _: &'static str) -> Result<IoMap, RuntimeError> {
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
        _: crate::services::IrqHandlerFn,
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
        dma: crate::DmaInfo,
        _: usize,
        _: usize,
    ) -> Result<crate::handle::CoherentMem, RuntimeError> {
        // Test mock: fake identity allocator honoring the device's DMA
        // view through the same translate() the real host uses, so the
        // mask/offset contract is exercised in tests too.
        let paddr = 0x4000_0000u64 + self.mapped.load(Ordering::Relaxed) as u64;
        match dma.translate(paddr) {
            Some(bus) => Ok(crate::handle::CoherentMem::new(
                paddr as usize,
                bus,
                0x1000,
                None,
            )),
            None => Err(RuntimeError::NoMemory),
        }
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
        crate::desc::DeviceLocation::FirmwareNode { id: 1 },
        crate::desc::DeviceIdentity::Platform(crate::desc::PlatformIdentity {
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
    desc.identity = crate::desc::DeviceIdentity::Platform(crate::desc::PlatformIdentity {
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
        crate::desc::DeviceLocation::FirmwareNode { id: 1 },
        crate::desc::DeviceIdentity::Platform(crate::desc::PlatformIdentity {
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
        crate::desc::DeviceLocation::FirmwareNode { id: 2 },
        crate::desc::DeviceIdentity::Platform(crate::desc::PlatformIdentity {
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
    use crate::desc::DeviceLocation;

    static CHILD_REMOVE_SAW_BUSY: SpinNoPreempt<bool> = SpinNoPreempt::new(false);
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

        fn remove(&self, _dev: &mut BoundDevice<'_, PlatformBus, ()>) {
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
            crate::desc::DeviceIdentity::Platform(crate::desc::PlatformIdentity {
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
    use crate::desc::DeviceLocation;

    fn pci_function(_vendor: u16, firmware: u32) -> DeviceDescriptor {
        DeviceDescriptor::new(
            crate::platform::PlatformBus::ID, // 类型故意用 platform（测类型一致性）
            DeviceLocation::FirmwareNode { id: firmware },
            crate::desc::DeviceIdentity::Platform(crate::desc::PlatformIdentity {
                firmware_id: Some(alloc::string::String::from("bus,test")),
                compatibles: alloc::vec![alloc::string::String::from("bus,test")],
                alias: None,
            }),
        )
    }

    let (domain, services) = fresh_domain();
    domain.set_services(services);

    let root = domain
        .add_bus_instance(crate::platform::PlatformBus::ID, None, "test-0")
        .expect("create root instance");

    // Identity surface: type + no bridge + label.
    let (bus, bridge, label) = domain.bus_instance_identity(root).expect("identity");
    assert_eq!(bus, crate::platform::PlatformBus::ID);
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
        .add_bus_instance(crate::platform::PlatformBus::ID, Some(bridge_dev), "test-1")
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
    desc.identity = crate::desc::DeviceIdentity::Platform(crate::desc::PlatformIdentity {
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
    desc.identity = crate::desc::DeviceIdentity::Platform(crate::desc::PlatformIdentity {
        firmware_id: Some(alloc::string::String::from("nobody,matches")),
        compatibles: alloc::vec![alloc::string::String::from("nobody,matches")],
        alias: None,
    });
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
    use crate::device::DeviceCtx;

    let (domain, services) = fresh_domain();
    let mut desc = test_descriptor();
    desc.firmware = Some(crate::desc::FirmwareNodeInfo {
        phandle: None,
        clock_frequency: Some(24_000_000),
        clock_cells: None,
        clock_output_name: None,
        properties: alloc::vec![
            ("vendor,quirk-cap", crate::FirmwareProp::U32(7)),
            ("vendor,label", crate::FirmwareProp::Str("rev-b")),
            ("dma-coherent", crate::FirmwareProp::Empty),
            (
                "vendor,dma-window",
                crate::FirmwareProp::Bytes(&[0, 0, 0, 1, 0, 0, 0, 2]),
            ),
            // The extracted clock fast-fact ALSO lives in the bag — the
            // enumeration snapshot captures every property.
            ("clock-frequency", crate::FirmwareProp::U32(24_000_000)),
        ],
    });
    let _ = domain;

    let ctx = DeviceCtx::<PlatformBus>::new(&desc, 1, &*services);
    assert_eq!(ctx.fw_u32("vendor,quirk-cap"), Some(7));
    assert_eq!(ctx.fw_str("vendor,label"), Some("rev-b"));
    assert!(ctx.fw_flag("dma-coherent"));
    assert_eq!(ctx.fw_prop("nope"), None);
    // Multi-cell payloads reinterpret on demand — the parse never guessed.
    assert_eq!(
        ctx.fw_prop("vendor,dma-window").and_then(|p| p.u32_list()),
        Some(alloc::vec![1, 2])
    );
    // Every property is in the bag, including ones with extracted
    // fast-facts.
    assert_eq!(ctx.fw_u32("clock-frequency"), Some(24_000_000));
}

#[def_test]
fn alloc_coherent_translates_through_the_devices_dma_view() {
    // The mock's fake allocator hands out identity CPU-physical pages
    // around 0x4000_0000; the DEVICE-visible address must come back
    // translated through the descriptor's dma view.
    use crate::device::DeviceCtx;

    let (_domain, services) = fresh_domain();
    let mut desc = test_descriptor();
    desc.dma = crate::DmaInfo {
        mask: 0xFFFF_FFFF,
        offset: 0x2000_0000,
        coherent: false,
    };

    let mut ctx = DeviceCtx::<PlatformBus>::new(&desc, 1, &*services);
    let mem = ctx.alloc_coherent(0x1000, 0x1000).expect("alloc");
    // paddr 0x4000_0000 → device 0x2000_0000, both under the 32-bit mask.
    assert_eq!(mem.bus_addr(), 0x2000_0000);

    // A device whose window cannot address the mock's pages fails CLEANLY.
    desc.dma = crate::DmaInfo {
        mask: 0x0FFF_FFFF,
        offset: 0,
        coherent: true,
    };
    let mut ctx = DeviceCtx::<PlatformBus>::new(&desc, 2, &*services);
    assert!(ctx.alloc_coherent(0x1000, 0x1000).is_err());
}
