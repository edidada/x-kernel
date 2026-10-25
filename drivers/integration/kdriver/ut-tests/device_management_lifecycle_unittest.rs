// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lifecycle regressions using the real host management-lock factory.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use driver_model::{
    AddDeviceError, Bus, BusInstanceId, ClkHandle, ClkSpec, ClockConsumerServices, CoherentMem,
    DevIrq, DeviceAccess, DeviceContext, DeviceDescriptor, DeviceDomain, DeviceManagementError,
    DeviceManagementLock, DeviceRef, DeviceResourceServices, DeviceServices, DeviceState,
    DriverDesc, DriverOps, Environment, ExtensionRegistry, ExtensionServices, IoMap, IrqHandlerFn,
    IrqSpec, MmioRegion, OfMatch, PlatformBus, PlatformBusData, PlatformId, ProbeError,
    ProviderRegistration, ProviderServices, RuntimeError, ServiceError, Virq,
};
use kcompletion::Completion;
use kspin::{IrqSave, NoPreempt};
use ksync::Mutex;
use unittest::{assert, assert_eq, def_test};

pub(super) struct HostServices {
    inner: Arc<dyn DeviceServices>,
    factory_calls: AtomicUsize,
}

impl HostServices {
    pub(super) fn new() -> Self {
        Self {
            inner: crate::host_services::new_device_services(Arc::new(UnavailableIrq)),
            factory_calls: AtomicUsize::new(0),
        }
    }
}

impl DeviceServices for HostServices {
    fn create_management_lock(&self) -> Result<Arc<dyn DeviceManagementLock>, RuntimeError> {
        self.factory_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.create_management_lock()
    }
}

impl ExtensionServices for HostServices {
    fn extensions(&self) -> &ExtensionRegistry {
        self.inner.extensions()
    }
}

impl ProviderServices for HostServices {
    fn register_provider(&self, registration: ProviderRegistration) -> Result<(), RuntimeError> {
        self.inner.register_provider(registration)
    }
}

impl ClockConsumerServices for HostServices {
    fn clk_get(&self, device: DeviceContext<'_>, spec: ClkSpec) -> Result<ClkHandle, ServiceError> {
        self.inner.clk_get(device, spec)
    }
}

impl DeviceResourceServices for HostServices {
    fn dma(
        &self,
        device: DeviceContext<'_>,
    ) -> Result<Arc<dyn driver_os_api::DeviceDma>, RuntimeError> {
        self.inner.dma(device)
    }

    fn iomap(
        &self,
        device: DeviceContext<'_>,
        region: MmioRegion,
        name: &'static str,
    ) -> Result<IoMap, RuntimeError> {
        self.inner.iomap(device, region, name)
    }

    fn request_irq(
        &self,
        device: DeviceContext<'_>,
        spec: IrqSpec,
        name: &'static str,
        handler: IrqHandlerFn,
    ) -> Result<DevIrq, RuntimeError> {
        self.inner.request_irq(device, spec, name, handler)
    }

    fn alloc_coherent(
        &self,
        device: DeviceContext<'_>,
        len: usize,
        align: usize,
    ) -> Result<CoherentMem, RuntimeError> {
        self.inner.alloc_coherent(device, len, align)
    }

    fn map_irq(&self, device: DeviceContext<'_>, spec: IrqSpec) -> Result<Virq, RuntimeError> {
        self.inner.map_irq(device, spec)
    }
}

struct UnavailableIrq;

impl driver_os_api::IrqOp for UnavailableIrq {
    fn request_irq(
        &self,
        _: driver_os_api::IrqResource,
        _: IrqHandlerFn,
    ) -> driver_os_api::ResResult<driver_os_api::IrqHandlerToken> {
        Err(driver_os_api::ResError::Unsupported)
    }

    fn release_irq(&self, _: driver_os_api::IrqResource, _: driver_os_api::IrqHandlerToken) {}

    fn set_irq_enabled(&self, _: driver_os_api::IrqResource, _: bool) {}

    fn map_irq(
        &self,
        _: driver_os_api::IrqRouteDesc,
    ) -> driver_os_api::ResResult<driver_os_api::IrqResource> {
        Err(driver_os_api::ResError::Unsupported)
    }
}

struct DmaProbe;

impl DriverOps<PlatformBus> for DmaProbe {
    type Data = CoherentMem;

    fn probe(
        &self,
        device: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
    ) -> Result<CoherentMem, ProbeError> {
        let memory = device.res().alloc_coherent(4097, 8192)?;
        let reference = device.reference();
        let view = reference
            .descriptor()
            .dma
            .as_ref()
            .ok_or(RuntimeError::BadState)?;
        let physical = khal::mem::v2p(memaddr::VirtAddr::from_usize(memory.as_ptr() as usize))
            .as_usize() as u64;
        if view.translate_range(physical, 8192) != Some(memory.bus_addr()) {
            return Err(RuntimeError::BadState.into());
        }
        Ok(memory)
    }
}

#[def_test]
fn model_callbacks_allocate_with_each_descriptor_dma_view() {
    let domain = DeviceDomain::new();
    domain.set_services(Arc::new(HostServices::new()));
    let root = domain
        .add_bus_instance(PlatformBus::ID, None, "host-dma-ut")
        .unwrap();
    static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("test,host-dma"))];
    domain.register_driver(DriverDesc::new("host-dma-ut", IDS, DmaProbe));
    for (node, bus_start) in [(1, 0x1_0000_0000), (2, 0x2_0000_0000)] {
        let descriptor = DeviceDescriptor::builder()
            .bus_instance(root)
            .bus_data(Box::new(PlatformBusData::firmware_node(
                node,
                &["test,host-dma"],
            )))
            .dma(driver_model::DmaInfo {
                window: driver_model::DmaWindow::new(0, bus_start, 0x1_0000_0000).unwrap(),
                ..driver_model::DmaInfo::DEFAULT
            })
            .build();
        let device = domain.add_device(descriptor).unwrap();
        domain.probe_pending();
        assert_eq!(domain.snapshot(&device).unwrap().state, DeviceState::Active);
        domain.unbind_device(&device).unwrap();
        domain.bind_device(&device).unwrap();
        assert_eq!(domain.snapshot(&device).unwrap().state, DeviceState::Active);
        domain.remove_device(&device).unwrap();
    }
}

struct HeldDmaProbe(Arc<Mutex<Vec<Result<(), ProbeError>>>>);

impl DriverOps<PlatformBus> for HeldDmaProbe {
    type Data = Arc<dyn driver_os_api::DeviceDma>;

    fn probe(
        &self,
        device: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
    ) -> Result<Self::Data, ProbeError> {
        let capability = device.res().dma();
        self.0
            .lock()
            .push(capability.as_ref().map(|_| ()).map_err(Clone::clone));
        capability
    }
}

fn held_dma_domain(probe: HeldDmaProbe) -> (DeviceDomain, BusInstanceId) {
    let domain = DeviceDomain::new();
    domain.set_services(Arc::new(HostServices::new()));
    let root = domain
        .add_bus_instance(PlatformBus::ID, None, "held-dma-ut")
        .unwrap();
    static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("test,held-dma"))];
    domain.register_driver(DriverDesc::new("held-dma-ut", IDS, probe));
    (domain, root)
}

fn held_dma_descriptor(
    root: BusInstanceId,
    node: u32,
    view: Option<driver_model::DmaInfo>,
) -> DeviceDescriptor {
    DeviceDescriptor::builder()
        .bus_instance(root)
        .bus_data(Box::new(PlatformBusData::firmware_node(
            node,
            &["test,held-dma"],
        )))
        .dma_opt(view)
        .build()
}

/// Exercise retained capabilities without starting hardware; release every native
/// allocation/mapping before returning observations to the test assertions.
fn exercise_held_dma(
    capability: &dyn driver_os_api::DeviceDma,
    view: driver_model::DmaInfo,
) -> driver_os_api::ResResult<bool> {
    let memory = capability.alloc_coherent(4097, 8192)?;
    let physical =
        khal::mem::v2p(memaddr::VirtAddr::from_usize(memory.as_ptr() as usize)).as_usize() as u64;
    let allocation_matches = view.translate_range(physical, 8192) == Some(memory.bus_addr());
    drop(memory);

    let mut bytes = [0x5a_u8; 128];
    let buffer = NonNull::from(&mut bytes[..]);
    // SAFETY: bytes remains live and exclusive until mapping is dropped below;
    // no CPU accesses occur while mapped, and no hardware receives its address.
    // The test runs and releases the guard in sleepable task context.
    let mapping =
        unsafe { capability.map_streaming(buffer, driver_os_api::DmaDirection::DriverToDevice) }?;
    let bus = mapping.bus_addr();
    let mapped_len = mapping.len();
    let translated = view
        .translate(0)
        .and_then(|bus_start| bus.checked_sub(bus_start))
        .and_then(|physical| view.translate_range(physical, mapped_len as u64));
    drop(mapping);
    Ok(allocation_matches
        && mapped_len == bytes.len()
        && translated == Some(bus)
        && bytes == [0x5a; 128])
}

#[def_test(serial)]
fn held_dma_keeps_two_device_views_after_callbacks_and_removal() {
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let (domain, root) = held_dma_domain(HeldDmaProbe(attempts.clone()));
    let first_view = driver_model::DmaInfo {
        window: driver_model::DmaWindow::new(0, 0x1_0000_0000, 0x1_0000_0000).unwrap(),
        ..driver_model::DmaInfo::DEFAULT
    };
    let second_view = driver_model::DmaInfo {
        window: driver_model::DmaWindow::new(0, 0x2_0000_0000, 0x1_0000_0000).unwrap(),
        ..driver_model::DmaInfo::DEFAULT
    };
    let first = domain
        .add_device(held_dma_descriptor(root, 1, Some(first_view)))
        .unwrap();
    let second = domain
        .add_device(held_dma_descriptor(root, 2, Some(second_view)))
        .unwrap();
    let first_dma = domain
        .with_data::<Arc<dyn driver_os_api::DeviceDma>, _>(&first, Arc::clone)
        .unwrap();
    let second_dma = domain
        .with_data::<Arc<dyn driver_os_api::DeviceDma>, _>(&second, Arc::clone)
        .unwrap();
    domain.set_services(Arc::new(HostServices::new()));
    let after_callbacks = [
        exercise_held_dma(first_dma.as_ref(), first_view),
        exercise_held_dma(second_dma.as_ref(), second_view),
    ];
    let removals = [domain.remove_device(&first), domain.remove_device(&second)];
    let registrations_gone =
        domain.snapshot(&first).is_none() && domain.snapshot(&second).is_none();
    drop(first);
    drop(second);
    drop(domain);
    let after_removal = [
        exercise_held_dma(second_dma.as_ref(), second_view),
        exercise_held_dma(first_dma.as_ref(), first_view),
    ];
    drop(first_dma);
    drop(second_dma);
    let attempts_snapshot = attempts.lock().clone();
    assert_eq!(attempts_snapshot.as_slice(), &[Ok(()), Ok(())]);
    assert_eq!(after_callbacks, [Ok(true), Ok(true)]);
    assert_eq!(removals, [Ok(()), Ok(())]);
    assert!(registrations_gone);
    assert_eq!(after_removal, [Ok(true), Ok(true)]);
}

#[def_test(serial)]
fn held_dma_acquisition_rejects_missing_and_noncoherent_views() {
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let (domain, root) = held_dma_domain(HeldDmaProbe(attempts.clone()));
    let missing = domain
        .add_device(held_dma_descriptor(root, 1, None))
        .unwrap();
    let noncoherent = domain
        .add_device(held_dma_descriptor(
            root,
            2,
            Some(driver_model::DmaInfo {
                coherent: false,
                ..driver_model::DmaInfo::DEFAULT
            }),
        ))
        .unwrap();
    let states = [
        domain.snapshot(&missing).map(|snapshot| snapshot.state),
        domain.snapshot(&noncoherent).map(|snapshot| snapshot.state),
    ];
    let removals = [
        domain.remove_device(&missing),
        domain.remove_device(&noncoherent),
    ];
    drop(domain);
    let attempts_snapshot = attempts.lock().clone();
    assert_eq!(
        attempts_snapshot.as_slice(),
        &[
            Err(ProbeError::Fatal(RuntimeError::Unsupported)),
            Err(ProbeError::Fatal(RuntimeError::Unsupported)),
        ]
    );
    assert_eq!(
        states,
        [Some(DeviceState::Unclaimed), Some(DeviceState::Unclaimed)]
    );
    assert_eq!(removals, [Ok(()), Ok(())]);
}

struct MmioProbe;

#[cfg(feature = "hwrng")]
#[def_test(serial)]
fn native_device_dma_works_through_virtio_backend_and_clone_release() {
    use virtio::{BufferDirection, DeviceDmaBackend, DmaBackend};

    let attempts = Arc::new(Mutex::new(Vec::new()));
    let (domain, root) = held_dma_domain(HeldDmaProbe(attempts));
    let device = domain
        .add_device(held_dma_descriptor(
            root,
            1,
            Some(driver_model::DmaInfo::DEFAULT),
        ))
        .unwrap();
    let capability = domain
        .with_data::<Arc<dyn driver_os_api::DeviceDma>, _>(&device, Arc::clone)
        .unwrap();
    domain.remove_device(&device).unwrap();
    drop(device);
    drop(domain);
    // SAFETY: this native QEMU identity capability uses non-sleeping allocation
    // and release and provides the same direct addresses in both VirtIO modes.
    // No hardware receives the addresses; all raw releases use original tuples.
    let backend = unsafe { DeviceDmaBackend::new(capability) };
    let clone = backend.clone();
    let (bus, pointer) = backend.allocate(1, BufferDirection::Both, true).unwrap();
    let physical =
        khal::mem::v2p(memaddr::VirtAddr::from_usize(pointer.as_ptr() as usize)).as_usize() as u64;
    let allocation_matches = bus == physical;
    // SAFETY: clone retains the allocating instance; this live original tuple
    // is released once, with no hardware or CPU users of the allocation.
    unsafe { clone.deallocate(bus, pointer, 1, BufferDirection::Both, true) };
    let mut original = [0xa5u8; 64];
    let buffer = NonNull::from(original.as_mut_slice());
    let request_lock = kspin::SpinNoIrq::new(());
    let request = request_lock.lock();
    // SAFETY: the original array stays live/exclusive through matching unshare;
    // this native capability supports request-lock context, and no DMA starts.
    let mapped = unsafe { backend.share(buffer, BufferDirection::DeviceToDriver, false) };
    let did_map = mapped.is_ok();
    if let Ok(address) = mapped {
        // SAFETY: this is the original active tuple, released once through a
        // clone of the same instance before the original array is accessed.
        unsafe { clone.unshare(address, buffer, BufferDirection::DeviceToDriver, false) };
    }
    drop(request);
    drop(backend);
    drop(clone);
    assert!(allocation_matches && did_map);
    assert_eq!(original, [0xa5; 64]);
}

impl DriverOps<PlatformBus> for MmioProbe {
    type Data = IoMap;

    fn probe(
        &self,
        device: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
    ) -> Result<IoMap, ProbeError> {
        device.res().iomap(0)
    }
}

#[def_test(serial)]
fn model_callbacks_release_mmio_across_unbind_and_rebind() {
    let physical_page = 0x0b01_0000;
    let domain = DeviceDomain::new();
    domain.set_services(Arc::new(HostServices::new()));
    let root = domain
        .add_bus_instance(PlatformBus::ID, None, "host-mmio-ut")
        .unwrap();
    static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("test,host-mmio"))];
    domain.register_driver(DriverDesc::new("host-mmio-ut", IDS, MmioProbe));
    let mut resources = driver_model::ResourceMap::new();
    resources.push_kind(driver_model::ResourceKind::Mmio(MmioRegion::new(
        physical_page,
        4096,
    )));
    let descriptor = DeviceDescriptor::builder()
        .bus_instance(root)
        .bus_data(Box::new(PlatformBusData::firmware_node(
            1,
            &["test,host-mmio"],
        )))
        .resources(resources)
        .build();
    let is_mapped =
        || memspace::device_regions().any(|entry| entry.paddr.as_usize() == physical_page);
    assert!(!is_mapped());
    let device = domain.add_device(descriptor).unwrap();
    domain.probe_pending();
    let first_mapping = is_mapped();
    domain.unbind_device(&device).unwrap();
    let first_release = !is_mapped();
    domain.bind_device(&device).unwrap();
    let second_mapping = is_mapped();
    domain.remove_device(&device).unwrap();
    assert!(first_mapping && first_release && second_mapping && !is_mapped());
}

type RemoveHook = Arc<dyn Fn(&DeviceRef) + Send + Sync>;
type ProbeHook = Arc<dyn Fn(&DeviceRef) + Send + Sync>;

struct Observations {
    probes: AtomicUsize,
    removes: AtomicUsize,
    drops: AtomicUsize,
    probe_hook: Mutex<Option<ProbeHook>>,
    remove_hook: Mutex<Option<RemoveHook>>,
}

impl Observations {
    fn counts(&self) -> (usize, usize, usize) {
        (
            self.probes.load(Ordering::Acquire),
            self.removes.load(Ordering::Acquire),
            self.drops.load(Ordering::Acquire),
        )
    }
}

struct TestData(Arc<Observations>);

impl Drop for TestData {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::Release);
    }
}

struct TestDriver(Arc<Observations>);

impl DriverOps<PlatformBus> for TestDriver {
    type Data = TestData;

    fn probe(
        &self,
        device: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
    ) -> Result<TestData, ProbeError> {
        self.0.probes.fetch_add(1, Ordering::Release);
        let hook = self.0.probe_hook.lock().clone();
        if let Some(hook) = hook {
            hook(&device.reference());
        }
        Ok(TestData(self.0.clone()))
    }

    fn remove(
        &self,
        device: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
        _: &mut TestData,
    ) {
        self.0.removes.fetch_add(1, Ordering::Release);
        let hook = self.0.remove_hook.lock().clone();
        if let Some(hook) = hook {
            hook(&device.reference());
        }
    }
}

struct Fixture {
    domain: Arc<DeviceDomain>,
    services: Arc<HostServices>,
    observations: Arc<Observations>,
    root: BusInstanceId,
}

impl Fixture {
    fn new() -> Result<Self, AddDeviceError> {
        let domain = Arc::new(DeviceDomain::new());
        let services = Arc::new(HostServices::new());
        domain.set_services(services.clone());
        let root = domain.add_bus_instance(PlatformBus::ID, None, "management-ut")?;
        let observations = Arc::new(Observations {
            probes: AtomicUsize::new(0),
            removes: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            probe_hook: Mutex::new(None),
            remove_hook: Mutex::new(None),
        });
        static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("test,management"))];
        domain.register_driver(DriverDesc::new(
            "management-ut",
            IDS,
            TestDriver(observations.clone()),
        ));
        Ok(Self {
            domain,
            services,
            observations,
            root,
        })
    }

    fn descriptor(&self, node: u32) -> DeviceDescriptor {
        DeviceDescriptor::builder()
            .bus_instance(self.root)
            .bus_data(Box::new(PlatformBusData::firmware_node(
                node,
                &["test,management"],
            )))
            .build()
    }

    fn add(&self, node: u32) -> Result<DeviceRef, AddDeviceError> {
        self.domain.add_device(self.descriptor(node))
    }
}

#[def_test(serial)]
fn factory_rejection_leaves_no_registered_or_pending_device() {
    let fixture = Fixture::new().unwrap();
    let descriptor = fixture.descriptor(1);
    let result = {
        let _guard = IrqSave::new();
        fixture.domain.add_device(descriptor)
    };
    assert!(matches!(
        result,
        Err(AddDeviceError::Management(RuntimeError::Management(
            DeviceManagementError::InvalidContext
        )))
    ));
    assert_eq!(fixture.services.factory_calls.load(Ordering::Relaxed), 1);
    assert!(fixture.domain.bus_instance_devices(fixture.root).is_empty());
    assert!(fixture.domain.deferred_report().is_empty());
    assert_eq!(
        fixture.domain.probe_pending(),
        driver_model::ProbeStats::default()
    );
    assert_eq!(fixture.observations.counts(), (0, 0, 0));
    let device = fixture.add(1).unwrap();
    assert_eq!(fixture.services.factory_calls.load(Ordering::Relaxed), 2);
    assert_eq!(fixture.domain.bus_instance_devices(fixture.root).len(), 1);
    assert_eq!(fixture.observations.counts(), (1, 0, 0));
    assert_eq!(fixture.domain.remove_device(&device), Ok(()));
    assert_eq!(fixture.observations.counts(), (1, 1, 1));
}

fn invalid_operations(domain: &DeviceDomain, device: &DeviceRef) -> bool {
    let results = [
        domain.unbind_device(device),
        domain.bind_device(device).map(|_| ()),
        domain.remove_device(device),
    ];
    results
        == [Err(RuntimeError::Management(
            DeviceManagementError::InvalidContext,
        )); 3]
}

#[def_test(serial)]
fn invalid_context_lifecycle_calls_preserve_binding_and_publication() {
    let fixture = Fixture::new().unwrap();
    let device = fixture.add(1).unwrap();
    for expected_state in [DeviceState::Active, DeviceState::Unclaimed] {
        let counts_before = fixture.observations.counts();
        let was_online = device.is_online();
        let driver_before = device.driver_name();
        let irq_rejected = {
            let _guard = IrqSave::new();
            invalid_operations(&fixture.domain, &device)
        };
        let preempt_rejected = {
            let _guard = NoPreempt::new();
            invalid_operations(&fixture.domain, &device)
        };
        let rcu_rejected = krcu::with_read(|_| invalid_operations(&fixture.domain, &device));
        assert!(irq_rejected && preempt_rejected && rcu_rejected);
        assert_eq!(fixture.observations.counts(), counts_before);
        assert_eq!(device.is_online(), was_online);
        assert_eq!(device.driver_name(), driver_before);
        assert_eq!(
            fixture.domain.snapshot(&device).unwrap().state,
            expected_state
        );
        assert_eq!(fixture.domain.bus_instance_devices(fixture.root).len(), 1);
        if expected_state == DeviceState::Active {
            assert_eq!(fixture.domain.unbind_device(&device), Ok(()));
            assert_eq!(fixture.observations.counts(), (1, 1, 1));
        }
    }
    assert_eq!(fixture.domain.bind_device(&device).unwrap().activated, 1);
    assert_eq!(fixture.observations.counts(), (2, 1, 1));
    assert_eq!(fixture.domain.remove_device(&device), Ok(()));
    assert_eq!(fixture.domain.remove_device(&device), Ok(()));
    assert!(fixture.domain.snapshot(&device).is_none());
    assert_eq!(fixture.observations.counts(), (2, 2, 2));
}

#[def_test(serial)]
fn remove_callback_cannot_reenter_same_device_management() {
    let fixture = Fixture::new().unwrap();
    let device = fixture.add(1).unwrap();
    let results = Arc::new(Mutex::new(None));
    *fixture.observations.remove_hook.lock() = Some(Arc::new({
        let domain = Arc::downgrade(&fixture.domain);
        let results = results.clone();
        move |device| {
            if let Some(domain) = domain.upgrade() {
                *results.lock() = Some([
                    domain.remove_device(device),
                    domain.unbind_device(device),
                    domain.bind_device(device).map(|_| ()),
                ]);
            }
        }
    }));
    let result = fixture.domain.remove_device(&device);
    *fixture.observations.remove_hook.lock() = None;
    assert_eq!(result, Ok(()));
    assert_eq!(
        *results.lock(),
        Some([Err(RuntimeError::Management(DeviceManagementError::Reentrant)); 3])
    );
    assert_eq!(fixture.observations.counts(), (1, 1, 1));
    assert!(fixture.domain.snapshot(&device).is_none());
    assert_eq!(fixture.domain.remove_device(&device), Ok(()));
}

pub(super) fn wait_bounded(mut ready: impl FnMut() -> bool) -> bool {
    let deadline_ns = khal::time::monotonic_time_nanos().saturating_add(5_000_000_000);
    loop {
        if ready() {
            return true;
        }
        if khal::time::monotonic_time_nanos() >= deadline_ns {
            return false;
        }
        ktask::yield_now();
    }
}

pub(super) fn join_bounded(task: &ktask::KtaskRef) -> Option<i32> {
    wait_bounded(|| task.state() == ktask::TaskState::Exited).then(|| task.join())
}

struct RemovalTask {
    task: ktask::KtaskRef,
    started: Arc<Completion>,
    finished: Arc<Completion>,
    result: Arc<Mutex<Option<Result<(), RuntimeError>>>>,
}

impl RemovalTask {
    fn spawn(domain: Arc<DeviceDomain>, device: DeviceRef) -> Self {
        let started = Arc::new(Completion::new());
        let finished = Arc::new(Completion::new());
        let result = Arc::new(Mutex::new(None));
        let task = ktask::spawn({
            let started = started.clone();
            let finished = finished.clone();
            let result = result.clone();
            move || {
                started.complete();
                let removal_result = domain.remove_device(&device);
                *result.lock() = Some(removal_result);
                finished.complete();
            }
        });
        Self {
            task,
            started,
            finished,
            result,
        }
    }
}

struct ConcurrentRemoval {
    owner_entered: bool,
    contender_started: bool,
    contender_blocked: bool,
    contender_finished_while_owned: bool,
    gate_released: bool,
    exit_codes: [Option<i32>; 2],
    results: [Option<Result<(), RuntimeError>>; 2],
}

fn concurrent_removal(
    fixture: &Fixture,
    first: &DeviceRef,
    second: &DeviceRef,
) -> ConcurrentRemoval {
    let entered = Arc::new(Completion::new());
    let release = Arc::new(Completion::new());
    let gate_released = Arc::new(AtomicBool::new(false));
    *fixture.observations.remove_hook.lock() = Some(Arc::new({
        let first = first.clone();
        let entered = entered.clone();
        let release = release.clone();
        let gate_released = gate_released.clone();
        move |device| {
            if device.same_device(&first) {
                entered.complete();
                gate_released.store(wait_bounded(|| release.is_completed()), Ordering::Release);
            }
        }
    }));
    let owner = RemovalTask::spawn(fixture.domain.clone(), first.clone());
    let owner_entered = wait_bounded(|| entered.try_wait());
    let contender = RemovalTask::spawn(fixture.domain.clone(), second.clone());
    let contender_started = wait_bounded(|| contender.started.try_wait());
    let contender_progressed = wait_bounded(|| {
        contender.task.state() == ktask::TaskState::Blocked || contender.finished.is_completed()
    });
    let contender_blocked =
        contender_progressed && contender.task.state() == ktask::TaskState::Blocked;
    let contender_finished_while_owned = contender.finished.is_completed();
    release.complete_all();
    let exit_codes = [join_bounded(&owner.task), join_bounded(&contender.task)];
    *fixture.observations.remove_hook.lock() = None;
    let results = [*owner.result.lock(), *contender.result.lock()];
    ConcurrentRemoval {
        owner_entered,
        contender_started,
        contender_blocked,
        contender_finished_while_owned,
        gate_released: gate_released.load(Ordering::Acquire),
        exit_codes,
        results,
    }
}

#[def_test(serial)]
fn concurrent_unregister_runs_remove_and_data_drop_once() {
    let fixture = Fixture::new().unwrap();
    let device = fixture.add(1).unwrap();
    let observation = concurrent_removal(&fixture, &device, &device);
    assert!(observation.owner_entered && observation.contender_started);
    assert!(observation.contender_blocked);
    assert!(!observation.contender_finished_while_owned);
    assert!(observation.gate_released);
    assert_eq!(observation.exit_codes, [Some(0), Some(0)]);
    assert_eq!(observation.results, [Some(Ok(())), Some(Ok(()))]);
    assert_eq!(fixture.observations.counts(), (1, 1, 1));
    assert!(fixture.domain.snapshot(&device).is_none());
    assert!(fixture.domain.bus_instance_devices(fixture.root).is_empty());
}

#[def_test(serial)]
fn different_devices_remove_callbacks_progress_independently() {
    let fixture = Fixture::new().unwrap();
    let first = fixture.add(1).unwrap();
    let second = fixture.add(2).unwrap();
    let observation = concurrent_removal(&fixture, &first, &second);
    assert!(observation.owner_entered && observation.contender_started);
    assert!(observation.contender_finished_while_owned);
    assert!(observation.gate_released);
    assert_eq!(observation.exit_codes, [Some(0), Some(0)]);
    assert_eq!(observation.results, [Some(Ok(())), Some(Ok(()))]);
    assert_eq!(fixture.observations.counts(), (2, 2, 2));
    assert!(fixture.domain.snapshot(&first).is_none());
    assert!(fixture.domain.snapshot(&second).is_none());
}

struct ProbeGate {
    observations: Arc<Observations>,
    entered: Arc<Completion>,
    release: Arc<Completion>,
    device: Arc<Mutex<Option<DeviceRef>>>,
    was_released: Arc<AtomicBool>,
}

impl ProbeGate {
    fn install(observations: Arc<Observations>) -> Self {
        let gate = Self {
            observations,
            entered: Arc::new(Completion::new()),
            release: Arc::new(Completion::new()),
            device: Arc::new(Mutex::new(None)),
            was_released: Arc::new(AtomicBool::new(false)),
        };
        *gate.observations.probe_hook.lock() = Some(Arc::new({
            let entered = gate.entered.clone();
            let release = gate.release.clone();
            let first_device = gate.device.clone();
            let was_released = gate.was_released.clone();
            move |device| {
                let is_first = {
                    let mut first = first_device.lock();
                    if first.is_none() {
                        *first = Some(device.clone());
                        true
                    } else {
                        false
                    }
                };
                if is_first {
                    entered.complete();
                    was_released.store(wait_bounded(|| release.is_completed()), Ordering::Release);
                }
            }
        }));
        gate
    }
}

impl Drop for ProbeGate {
    fn drop(&mut self) {
        self.release.complete_all();
        *self.observations.probe_hook.lock() = None;
    }
}

struct RegistrationTask {
    task: ktask::KtaskRef,
    finished: Arc<Completion>,
    result: Arc<Mutex<Option<Result<DeviceRef, AddDeviceError>>>>,
}

impl RegistrationTask {
    fn spawn(domain: Arc<DeviceDomain>, descriptor: DeviceDescriptor) -> Self {
        let finished = Arc::new(Completion::new());
        let result = Arc::new(Mutex::new(None));
        let task = ktask::spawn({
            let finished = finished.clone();
            let result = result.clone();
            move || {
                let registration_result = domain.add_device(descriptor);
                *result.lock() = Some(registration_result);
                finished.complete();
            }
        });
        Self {
            task,
            finished,
            result,
        }
    }

    fn device(&self) -> Option<DeviceRef> {
        self.result
            .lock()
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .cloned()
    }
}

#[def_test(serial)]
fn different_devices_probe_callbacks_progress_independently() {
    let fixture = Fixture::new().unwrap();
    let gate = ProbeGate::install(fixture.observations.clone());
    let first = RegistrationTask::spawn(fixture.domain.clone(), fixture.descriptor(1));
    let first_entered = wait_bounded(|| gate.entered.try_wait());
    let second = RegistrationTask::spawn(fixture.domain.clone(), fixture.descriptor(2));
    let second_finished_while_first_waited = wait_bounded(|| second.finished.is_completed());
    let second_state = second
        .device()
        .and_then(|device| fixture.domain.snapshot(&device))
        .map(|snapshot| snapshot.state);
    let first_state = gate
        .device
        .lock()
        .as_ref()
        .and_then(|device| fixture.domain.snapshot(device))
        .map(|snapshot| snapshot.state);
    let first_finished_early = first.finished.is_completed();
    gate.release.complete_all();
    let exit_codes = [join_bounded(&first.task), join_bounded(&second.task)];
    let gate_released = gate.was_released.load(Ordering::Acquire);
    drop(gate);
    let cleanup = if exit_codes == [Some(0), Some(0)] {
        [first.device(), second.device()]
            .map(|device| device.map(|device| fixture.domain.remove_device(&device)))
    } else {
        [None, None]
    };
    assert!(first_entered);
    assert!(second_finished_while_first_waited);
    assert!(!first_finished_early);
    assert_eq!(first_state, Some(DeviceState::Probing));
    assert_eq!(second_state, Some(DeviceState::Active));
    assert!(gate_released);
    assert_eq!(exit_codes, [Some(0), Some(0)]);
    assert_eq!(cleanup, [Some(Ok(())), Some(Ok(()))]);
    assert_eq!(fixture.observations.counts(), (2, 2, 2));
    assert!(fixture.domain.bus_instance_devices(fixture.root).is_empty());
}

#[def_test(serial)]
fn unregister_waits_for_same_device_probe_then_drops_data_once() {
    let fixture = Fixture::new().unwrap();
    let gate = ProbeGate::install(fixture.observations.clone());
    let registration = RegistrationTask::spawn(fixture.domain.clone(), fixture.descriptor(1));
    let probe_entered = wait_bounded(|| gate.entered.try_wait());
    let device = gate.device.lock().clone();
    let remover = device
        .as_ref()
        .map(|device| RemovalTask::spawn(fixture.domain.clone(), device.clone()));
    let remover_started = remover
        .as_ref()
        .is_some_and(|remover| wait_bounded(|| remover.started.try_wait()));
    let remover_blocked = remover.as_ref().is_some_and(|remover| {
        wait_bounded(|| {
            remover.task.state() == ktask::TaskState::Blocked || remover.finished.is_completed()
        }) && remover.task.state() == ktask::TaskState::Blocked
    });
    let counts_before_release = fixture.observations.counts();
    let state_before_release = device
        .as_ref()
        .and_then(|device| fixture.domain.snapshot(device))
        .map(|snapshot| snapshot.state);
    gate.release.complete_all();
    let registration_exit = join_bounded(&registration.task);
    let remover_exit = remover
        .as_ref()
        .and_then(|remover| join_bounded(&remover.task));
    let gate_released = gate.was_released.load(Ordering::Acquire);
    drop(gate);
    let removal_result = remover.as_ref().and_then(|remover| *remover.result.lock());
    let counts_after_removal = fixture.observations.counts();
    let is_unregistered = device
        .as_ref()
        .is_some_and(|device| fixture.domain.snapshot(device).is_none());
    if registration_exit == Some(0)
        && (remover.is_none() || remover_exit == Some(0))
        && let Some(device) = registration.device()
    {
        let _ = fixture.domain.remove_device(&device);
    }
    assert!(probe_entered && remover_started && remover_blocked);
    assert_eq!(counts_before_release, (1, 0, 0));
    assert_eq!(state_before_release, Some(DeviceState::Probing));
    assert!(gate_released);
    assert_eq!(registration_exit, Some(0));
    assert_eq!(remover_exit, Some(0));
    assert!(registration.device().is_some());
    assert_eq!(removal_result, Some(Ok(())));
    assert_eq!(counts_after_removal, (1, 1, 1));
    assert!(is_unregistered);
}

#[def_test(serial)]
fn explicit_bind_waits_for_same_device_probe_without_probing_twice() {
    let fixture = Fixture::new().unwrap();
    let gate = ProbeGate::install(fixture.observations.clone());
    let registration = RegistrationTask::spawn(fixture.domain.clone(), fixture.descriptor(1));
    let probe_entered = wait_bounded(|| gate.entered.try_wait());
    let device = gate.device.lock().clone();
    let started = Arc::new(Completion::new());
    let result = Arc::new(Mutex::new(None));
    let binder = device.as_ref().map(|device| {
        ktask::spawn({
            let domain = fixture.domain.clone();
            let device = device.clone();
            let started = started.clone();
            let result = result.clone();
            move || {
                started.complete();
                let binding = domain.bind_device(&device);
                *result.lock() = Some(binding.map(|stats| stats.activated));
            }
        })
    });
    let binder_started = binder.is_some() && wait_bounded(|| started.try_wait());
    let binder_blocked = binder.as_ref().is_some_and(|task| {
        wait_bounded(|| {
            task.state() == ktask::TaskState::Blocked || task.state() == ktask::TaskState::Exited
        }) && task.state() == ktask::TaskState::Blocked
    });
    let counts_before_release = fixture.observations.counts();
    gate.release.complete_all();
    let registration_exit = join_bounded(&registration.task);
    let binder_exit = binder.as_ref().and_then(join_bounded);
    let gate_released = gate.was_released.load(Ordering::Acquire);
    drop(gate);
    let counts_after_binding = fixture.observations.counts();
    let cleanup = if registration_exit == Some(0) && binder_exit == Some(0) {
        registration
            .device()
            .map(|device| fixture.domain.remove_device(&device))
    } else {
        None
    };
    assert!(probe_entered && binder_started && binder_blocked && gate_released);
    assert_eq!(counts_before_release, (1, 0, 0));
    assert_eq!(registration_exit, Some(0));
    assert_eq!(binder_exit, Some(0));
    assert_eq!(*result.lock(), Some(Ok(0)));
    assert_eq!(counts_after_binding, (1, 0, 0));
    assert_eq!(cleanup, Some(Ok(())));
    assert_eq!(fixture.observations.counts(), (1, 1, 1));
}
