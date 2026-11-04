// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scoped QEMU virt MMIO RNG binding using model-owned host capabilities.

use alloc::sync::Arc;
use core::mem::ManuallyDrop;

use driver_base::{DeviceKind, DriverError};
use driver_model::{
    BusInstanceId, DeviceAccess, DeviceDomain, DmaInfo, DriverDesc, DriverOps, Environment, IoMap,
    OfMatch, PlatformBus, PlatformId, ProbeError, RuntimeError,
};
use subsys_hwrng::{HWRNG, HwrngRegistration};
use virtio::{DeviceDmaBackend, DmaBackend, MmioTransport, MmioVersion, Transport, VirtIoRngDev};

use super::glue::VirtIoHalImpl;
use crate::rng_completion::{new_rng_completion, new_rng_drain};

type Runtime<T = MmioTransport<'static>, B = DeviceDmaBackend> = VirtIoRngDev<VirtIoHalImpl, T, B>;

static MATCH_IDS: &[PlatformId] = &[PlatformId::Of(OfMatch::compatible("virtio,mmio"))];

/// Build the registration record; the caller chooses the existing domain.
///
/// This neither installs services nor registers devices or publishes an RNG.
///
/// # Safety
/// Every matching device on `instance` must be exclusively routed from
/// an available direct QEMU Arm virt MMIO node, with no IOMMU or platform sharing
/// requirement. Its host services must be the native providers whose DMA calls
/// are non-sleeping under the RNG request lock and in IRQ context. Enumeration
/// must exclude legacy ownership, concurrent MMIO classification and overlapping
/// transports for the entire binding lifetime. These provenance guarantees must
/// also hold for later additions and rebinds. Probe checks coherent identity DMA
/// and modern RNG identification, but cannot infer platform policy from a header.
pub(crate) unsafe fn driver_desc(instance: BusInstanceId) -> DriverDesc<PlatformBus, RngDriver> {
    DriverDesc::new("virtio-rng-mmio-model", MATCH_IDS, RngDriver { instance })
}

/// Register this binding on the supplied existing domain, without enumeration.
///
/// # Safety
/// The domain's matching devices and host services must satisfy [`driver_desc`]
/// for all future additions and rebinds. Registration can probe existing devices.
pub(crate) unsafe fn register(domain: &DeviceDomain, instance: BusInstanceId) -> bool {
    // SAFETY: the caller guarantees the same exclusive QEMU/native-host scope
    // required by driver_desc for every device this domain presents to the ops.
    domain.register_driver(unsafe { driver_desc(instance) })
}

pub(crate) struct RngDriver {
    instance: BusInstanceId,
}

impl DriverOps<PlatformBus> for RngDriver {
    type Data = RngBinding;

    fn probe(
        &self,
        dev: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
    ) -> Result<Self::Data, ProbeError> {
        validate_instance(self.instance, dev.reference().descriptor().bus_instance)?;
        if !cfg!(target_arch = "aarch64") {
            return Err(RuntimeError::Unsupported.into());
        }
        if !ktask::is_sleepable_context() {
            return Err(RuntimeError::BadState.into());
        }
        validate_dma(dev.reference().descriptor().dma.as_ref())?;
        let irq = dev.res().irq(0)?;
        let drain = new_rng_drain().map_err(|_| RuntimeError::BadState)?;
        let capability = dev.res().dma()?;
        let mmio = dev.res().iomap(0)?;
        // SAFETY: the native IoMap owns the complete register mapping. The
        // unsafe registration contract excludes another transport/classifier;
        // this borrow finishes before constructing the exclusive transport.
        unsafe { validate_mmio(&mmio) }?;
        // SAFETY: registration confines the device to QEMU without IOMMU;
        // validate_dma requires a coherent identity window. Native services
        // support request-lock/IRQ acquisition and release. Both negotiated
        // access_platform modes therefore use the same physical DMA addresses.
        let backend = unsafe { DeviceDmaBackend::new(capability) };
        let mut mmio = ManuallyDrop::new(mmio);
        // SAFETY: validation proved a modern RNG header and a valid extent.
        // Exclusive ownership was required at registration. The manually held
        // mapping survives constructor/teardown panic and all transport access.
        let transport = unsafe { virtio::probe_mmio_device(mmio.as_ptr(), mmio.region().size) };
        let Some((_, transport)) = transport else {
            // SAFETY: no transport or DMA owner was returned by this constructor.
            unsafe { ManuallyDrop::drop(&mut mmio) };
            return Err(RuntimeError::Invalid.into());
        };
        let runtime = Runtime::try_new_with_irq_factory(
            transport,
            backend,
            new_rng_completion(),
            |handler| {
                dev.res()
                    .request_irq(irq, "virtio-rng-mmio-model", move |irq| handler.handle(irq))
                    .map_err(|_| DriverError::Io)
            },
            ktask::yield_now,
        );
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(error) => {
                // SAFETY: the constructor's error contract has released queues
                // and transport, confirming reset on IRQ registration rollback.
                unsafe { ManuallyDrop::drop(&mut mmio) };
                return Err(runtime_error(error).into());
            }
        };
        let mut binding = RngBinding {
            runtime,
            registration: None,
            mmio,
            is_stopped: false,
        };
        let registration =
            match HWRNG.register(dev.publication_source(), binding.runtime.clone(), drain) {
                Ok(registration) => registration,
                Err(_) => {
                    binding.stop_or_fail_closed();
                    return Err(RuntimeError::Io.into());
                }
            };
        binding.registration = Some(registration);
        Ok(binding)
    }

    fn remove(
        &self,
        _: &mut DeviceAccess<'_, PlatformBus>,
        _: &Environment<'_>,
        data: &mut Self::Data,
    ) {
        data.stop_or_fail_closed();
    }
}

/// Binding ownership; stale consumers retain only the closed runtime, not MMIO.
pub(crate) struct RngBinding<
    T: Transport = MmioTransport<'static>,
    B: DmaBackend = DeviceDmaBackend,
> {
    runtime: Arc<Runtime<T, B>>,
    registration: Option<HwrngRegistration>,
    mmio: ManuallyDrop<IoMap>,
    is_stopped: bool,
}

impl<T: Transport, B: DmaBackend> RngBinding<T, B> {
    fn shutdown(&mut self) -> Result<(), RuntimeError> {
        if self.is_stopped {
            return Ok(());
        }
        self.runtime.close_reads();
        if let Some(registration) = &mut self.registration {
            registration
                .unregister()
                .map_err(|_| RuntimeError::BadState)?;
        }
        self.runtime
            .finish_stop(ktask::yield_now)
            .map_err(runtime_error)?;
        self.is_stopped = true;
        Ok(())
    }

    fn stop_or_fail_closed(&mut self) {
        match self.shutdown() {
            Ok(()) => {}
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: TODO(cg): model remove and Drop \
                          cannot return cleanup failure; retain MMIO rather than reclaim active \
                          resources until a fallible teardown owner can retain the binding"
            )]
            Err(error) => {
                panic!("virtio RNG model teardown did not establish quiescence: {error:?}")
            }
        }
    }
}

impl<T: Transport, B: DmaBackend> Drop for RngBinding<T, B> {
    fn drop(&mut self) {
        self.stop_or_fail_closed();
        // SAFETY: shutdown drained publication, confirmed reset, drained IRQs
        // and destroyed the transport. It is now safe to unmap exactly once,
        // even if consumers retain references to the permanently closed runtime.
        unsafe { ManuallyDrop::drop(&mut self.mmio) };
    }
}

fn validate_instance(expected: BusInstanceId, actual: BusInstanceId) -> Result<(), RuntimeError> {
    if actual != expected {
        return Err(RuntimeError::Unsupported);
    }
    Ok(())
}

fn validate_dma(dma: Option<&DmaInfo>) -> Result<(), RuntimeError> {
    let dma = dma.ok_or(RuntimeError::Unsupported)?;
    if !dma.coherent || !dma.window.is_identity() {
        return Err(RuntimeError::Unsupported);
    }
    if dma.mask != u64::MAX && dma.mask & (dma.mask + 1) != 0 {
        return Err(RuntimeError::Invalid);
    }
    Ok(())
}

/// Reject unsupported identification without constructing a resetting transport.
///
/// # Safety
/// The mapping must cover real readable VirtIO identification registers or an
/// initialized fixture, with no concurrent mutation, classifier or transport.
unsafe fn validate_mmio(mmio: &IoMap) -> Result<(), RuntimeError> {
    // SAFETY: the caller supplies an exclusively held live mapping; classify
    // validates extent/alignment and reads only identification, without reset.
    match unsafe { virtio::classify_mmio_device(mmio.as_ptr(), mmio.region().size) } {
        Some((DeviceKind::Hwrng, MmioVersion::Modern)) => Ok(()),
        _ => Err(RuntimeError::Unsupported),
    }
}

fn runtime_error(error: DriverError) -> RuntimeError {
    match error {
        DriverError::NoMemory => RuntimeError::NoMemory,
        DriverError::Unsupported => RuntimeError::Unsupported,
        DriverError::InvalidInput => RuntimeError::Invalid,
        DriverError::BadState => RuntimeError::BadState,
        _ => RuntimeError::Io,
    }
}

unittest::path_mod_test!(tests, "../../../ut-tests/virtio_rng_model.rs");
