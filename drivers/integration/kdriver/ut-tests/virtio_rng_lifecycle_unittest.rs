// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real RNG runtime teardown through model callbacks, using controlled hardware.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use driver_model::{
    Bus, DeviceAccess, DeviceDescriptor, DeviceDomain, DeviceState, DriverDesc, DriverOps,
    Environment, OfMatch, PlatformBus, PlatformBusData, PlatformId, ProbeError, RuntimeError,
};
use driver_os_api::{
    IrqAction, IrqHandlerFn, IrqHandlerToken, IrqOp, IrqResource, IrqTrigger, ResResult, Virq,
};
use kerrno::{KError, KResult};
use kspin::{IrqSave, SpinNoIrq, SpinNoPreempt};
use subsys_hwrng::{HwrngHandle, HwrngRegistration, HwrngRegistry};
use unittest::{assert, assert_eq, def_test};
use virtio::{
    BufferDirection, PhysAddr, VirtIoHal, VirtIoRngDev,
    mock_virtio::{MockHal, MockReset, MockTransport},
};

use crate::rng_completion::{new_rng_completion, new_rng_drain};

static RINGS: AtomicUsize = AtomicUsize::new(0);
static MAPPINGS: AtomicUsize = AtomicUsize::new(0);
static UNSHARES: AtomicUsize = AtomicUsize::new(0);
static RELEASES: AtomicUsize = AtomicUsize::new(0);
static HANDLER: SpinNoIrq<Option<IrqHandlerFn>> = SpinNoIrq::new(None);
static REGISTRY: HwrngRegistry = HwrngRegistry::new();
static IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible(
    "test,actual-virtio-rng",
))];

struct TrackingHal;

// SAFETY: All allocations and mapping tuples are forwarded unchanged to the
// identity-mapped test HAL; counters do not alter allocation ownership or bounds.
unsafe impl VirtIoHal for TrackingHal {
    fn dma_alloc(
        pages: usize,
        direction: BufferDirection,
        platform: bool,
    ) -> (PhysAddr, NonNull<u8>) {
        let allocation = MockHal::dma_alloc(pages, direction, platform);
        RINGS.fetch_add(1, Ordering::Relaxed);
        allocation
    }

    unsafe fn dma_dealloc(
        address: PhysAddr,
        pointer: NonNull<u8>,
        pages: usize,
        platform: bool,
    ) -> i32 {
        // SAFETY: The queue supplies the same live allocation tuple returned by dma_alloc.
        let result = unsafe { MockHal::dma_dealloc(address, pointer, pages, platform) };
        RINGS.fetch_sub(1, Ordering::Relaxed);
        result
    }

    unsafe fn mmio_phys_to_virt(address: PhysAddr, size: usize) -> NonNull<u8> {
        // SAFETY: The caller supplies the valid identity-mapped test region.
        unsafe { MockHal::mmio_phys_to_virt(address, size) }
    }

    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection, platform: bool) -> PhysAddr {
        // SAFETY: The RNG owns this stable buffer until completion or confirmed cancellation.
        let address = unsafe { MockHal::share(buffer, direction, platform) };
        MAPPINGS.fetch_add(1, Ordering::Relaxed);
        address
    }

    unsafe fn unshare(
        address: PhysAddr,
        buffer: NonNull<[u8]>,
        direction: BufferDirection,
        platform: bool,
    ) {
        // SAFETY: The RNG returns its original buffer/mapping after hardware access has ended.
        unsafe { MockHal::unshare(address, buffer, direction, platform) };
        MAPPINGS.fetch_sub(1, Ordering::Relaxed);
        UNSHARES.fetch_add(1, Ordering::Relaxed);
    }
}

unittest::path_mod_test!(notify_tail, "virtio_rng_notify_tail.rs");

struct TestIrq;

impl IrqOp for TestIrq {
    fn request_irq(
        &self,
        resource: IrqResource,
        handler: IrqHandlerFn,
    ) -> ResResult<IrqHandlerToken> {
        if resource != IrqResource::plain(Virq::new(7), IrqTrigger::LevelHigh) {
            return Err(driver_os_api::ResError::InvalidResource);
        }
        *HANDLER.lock() = Some(handler);
        Ok(IrqHandlerToken::shared_action(1))
    }

    fn release_irq(&self, _: IrqResource, token: IrqHandlerToken) {
        core::assert_eq!(token, IrqHandlerToken::shared_action(1));
        let handler = HANDLER.lock().take();
        if let Some(handler) = handler {
            core::assert_eq!(handler(), IrqAction::NotHandled);
        }
        RELEASES.fetch_add(1, Ordering::Relaxed);
    }

    fn set_irq_enabled(&self, _: IrqResource, _: bool) {}
}

fn wait_for(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = khal::time::monotonic_time_nanos().saturating_add(5_000_000_000);
    loop {
        if ready() {
            return true;
        }
        if khal::time::monotonic_time_nanos() >= deadline {
            return false;
        }
        ktask::yield_now();
    }
}

type Runtime = VirtIoRngDev<TrackingHal, MockTransport>;

struct Observations {
    fail_after_publish: bool,
    reset: Arc<MockReset>,
    handles: SpinNoPreempt<Vec<HwrngHandle>>,
    removes: AtomicUsize,
    data_drops: AtomicUsize,
    cleanup_failed: AtomicBool,
    submitted_pending: AtomicBool,
    reader: SpinNoPreempt<Option<ktask::KtaskRef>>,
    reader_returned_closed: AtomicBool,
}

impl Observations {
    fn new(fail_after_publish: bool) -> Arc<Self> {
        Arc::new(Self {
            fail_after_publish,
            reset: Arc::new(MockReset::default()),
            handles: SpinNoPreempt::new(Vec::new()),
            removes: AtomicUsize::new(0),
            data_drops: AtomicUsize::new(0),
            cleanup_failed: AtomicBool::new(false),
            submitted_pending: AtomicBool::new(false),
            reader: SpinNoPreempt::new(None),
            reader_returned_closed: AtomicBool::new(false),
        })
    }
}

struct Binding {
    runtime: Arc<Runtime>,
    registration: Option<HwrngRegistration>,
    observations: Arc<Observations>,
}

impl Binding {
    fn shutdown(&mut self) -> KResult {
        self.runtime.close_reads();
        if let Some(registration) = &mut self.registration {
            registration.unregister()?;
        }
        self.runtime
            .finish_stop(ktask::yield_now)
            .map_err(|_| KError::Io)
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        self.observations.data_drops.fetch_add(1, Ordering::Relaxed);
    }
}

struct TestDriver(Arc<Observations>);

impl DriverOps<PlatformBus> for TestDriver {
    type Data = Binding;

    fn probe(
        &self,
        dev: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
    ) -> Result<Binding, ProbeError> {
        let drain = new_rng_drain().map_err(|_| RuntimeError::BadState)?;
        let mut transport = MockTransport::new_rng();
        transport.reset = Some(self.0.reset.clone());
        let runtime = Runtime::try_new_with_irq_factory(
            transport,
            virtio::StaticHal::default(),
            new_rng_completion(),
            |handler| {
                dev.res()
                    .request_irq_at(0, move || handler())
                    .map_err(|_| driver_base::DriverError::Io)
            },
            ktask::yield_now,
        )
        .map_err(|_| RuntimeError::Io)?;
        let mut data = Binding {
            runtime,
            registration: None,
            observations: self.0.clone(),
        };
        let registration =
            match REGISTRY.register(dev.publication_source(), data.runtime.clone(), drain) {
                Ok(registration) => registration,
                Err(_) => {
                    data.shutdown().map_err(|_| RuntimeError::Io)?;
                    return Err(RuntimeError::Io.into());
                }
            };
        let handle = registration.publication();
        let mappings = MAPPINGS.load(Ordering::Relaxed);
        let reader = ktask::spawn({
            let handle = handle.clone();
            let observations = self.0.clone();
            move || {
                let mut output = [0xaa; 8];
                let result = handle.read(&mut output);
                observations.reader_returned_closed.store(
                    result == Err(KError::BadState) && output == [0xaa; 8],
                    Ordering::Release,
                );
            }
        });
        let is_blocked = wait_for(|| {
            MAPPINGS.load(Ordering::Relaxed) == mappings + 1
                && reader.state() == ktask::TaskState::Blocked
        });
        self.0
            .submitted_pending
            .store(is_blocked, Ordering::Relaxed);
        *self.0.reader.lock() = Some(reader);
        self.0.handles.lock().push(handle);
        data.registration = Some(registration);
        if self.0.fail_after_publish {
            data.shutdown().map_err(|_| RuntimeError::Io)?;
            return Err(RuntimeError::Io.into());
        }
        Ok(data)
    }

    fn remove(
        &self,
        _: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
        data: &mut Binding,
    ) {
        self.0.removes.fetch_add(1, Ordering::Relaxed);
        self.0
            .cleanup_failed
            .store(data.shutdown().is_err(), Ordering::Relaxed);
    }
}

#[def_test(serial)]
fn concrete_rng_model_remove_and_failed_probe_release_dma_with_old_handles_alive() {
    for fail_after_publish in [false, true] {
        let rings = RINGS.load(Ordering::Relaxed);
        let mappings = MAPPINGS.load(Ordering::Relaxed);
        let releases = RELEASES.load(Ordering::Relaxed);
        let domain = DeviceDomain::new();
        domain.set_services(crate::host::new_device_services(Arc::new(TestIrq)));
        let root = domain
            .add_bus_instance(PlatformBus::ID, None, "rng-model-ut")
            .unwrap();
        let observations = Observations::new(fail_after_publish);
        domain.register_driver(DriverDesc::new(
            "actual-rng-ut",
            IDS,
            TestDriver(observations.clone()),
        ));
        let mut resources = driver_model::ResourceMap::new();
        resources.push_kind(driver_model::ResourceKind::Irq(
            driver_model::IrqSpec::resolved(Virq::new(7), IrqTrigger::LevelHigh),
        ));
        let device = domain
            .add_device(
                DeviceDescriptor::builder()
                    .bus_instance(root)
                    .bus_data(Box::new(PlatformBusData::firmware_node(
                        1,
                        &["test,actual-virtio-rng"],
                    )))
                    .resources(resources)
                    .build(),
            )
            .unwrap();
        let state = domain.snapshot(&device).unwrap().state;
        let pending_before_remove = MAPPINGS.load(Ordering::Relaxed);
        let remove_result = domain.unregister_device(&device);
        drop(domain);
        let reader = observations.reader.lock().take().unwrap();
        let reader_exit =
            wait_for(|| reader.state() == ktask::TaskState::Exited).then(|| reader.join());
        let handles = core::mem::take(&mut *observations.handles.lock());
        let stale_read = handles.first().map(|handle| handle.read(&mut [0; 8]));
        let rings_with_handles = RINGS.load(Ordering::Relaxed);
        let mappings_with_handles = MAPPINGS.load(Ordering::Relaxed);
        let releases_with_handles = RELEASES.load(Ordering::Relaxed);
        let driver_name = handles.first().and_then(HwrngHandle::driver_name);
        {
            let _guard = IrqSave::new();
            drop(handles);
        }
        assert_eq!(remove_result, Ok(()));
        assert_eq!(reader_exit, Some(0));
        assert!(observations.reader_returned_closed.load(Ordering::Acquire));
        assert_eq!(
            state,
            if fail_after_publish {
                DeviceState::Unclaimed
            } else {
                DeviceState::Bound
            }
        );
        assert!(observations.submitted_pending.load(Ordering::Relaxed));
        assert_eq!(
            pending_before_remove,
            mappings + usize::from(!fail_after_publish)
        );
        assert_eq!(
            observations.removes.load(Ordering::Relaxed),
            usize::from(!fail_after_publish)
        );
        assert_eq!(observations.data_drops.load(Ordering::Relaxed), 1);
        assert!(!observations.cleanup_failed.load(Ordering::Relaxed));
        assert_eq!(observations.reset.requests.load(Ordering::Relaxed), 1);
        assert_eq!(stale_read, Some(Err(KError::BadState)));
        assert_eq!(driver_name, Some("actual-rng-ut"));
        assert_eq!(rings_with_handles, rings);
        assert_eq!(mappings_with_handles, mappings);
        assert_eq!(releases_with_handles, releases + 1);
        assert_eq!(RELEASES.load(Ordering::Relaxed), releases_with_handles);
        assert!(HANDLER.lock().is_none());
        assert!(REGISTRY.devices().is_empty());
    }
}
