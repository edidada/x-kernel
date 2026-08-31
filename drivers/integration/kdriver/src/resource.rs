// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver-facing resource helpers backed by the [`device_res`] provider model.
//!
//! The X-Kernel provider implementation lives in `device-res-xkernel`. This
//! module keeps the driver orchestration crate's `DriverResult` boundary while
//! acquiring resources through the OS-neutral `device_res` API.

use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;

use device_res::{
    DmaSpec, IrqHandler as DevIrqHandler, IrqResource, IrqThreadHandler as DevIrqThreadHandler,
    MmioRegion, ResError, ResResult,
};
use device_res_xkernel::XKernelResourceProvider;
use driver_base::{DriverError, DriverResult};
use kdevice::DeviceObject;

static XKERNEL_RESOURCE_PROVIDER: XKernelResourceProvider = XKernelResourceProvider::new();

pub(crate) fn resource_provider() -> &'static XKernelResourceProvider {
    &XKERNEL_RESOURCE_PROVIDER
}

fn map_res_err(err: ResError) -> DriverError {
    match err {
        ResError::InvalidResource => DriverError::InvalidInput,
        ResError::MappingFailed => DriverError::Io,
        ResError::NoMemory => DriverError::NoMemory,
        ResError::Busy => DriverError::ResourceBusy,
        ResError::Unsupported => DriverError::Unsupported,
        ResError::NoProvider | ResError::NotFound => DriverError::InvalidInput,
    }
}

/// Map an MMIO region and tie its lifetime to `device`.
///
/// The mapping is released when the device's probe fails or the device is
/// removed. Returns the virtual base address for the region.
pub(crate) fn devm_iomap(
    device: &DeviceObject,
    region: MmioRegion,
    name: &'static str,
) -> DriverResult<NonNull<u8>> {
    device_res::devm_iomap_with_provider(resource_provider(), device, region, name)
        .map_err(map_res_err)
}

/// Register a context-carrying interrupt handler and tie its lifetime to
/// `device`.
///
/// The handler is released when the device's probe fails or the device is
/// removed.
pub(crate) fn devm_request_irq(
    device: &DeviceObject,
    irq: IrqResource,
    handler: Arc<dyn DevIrqHandler>,
) -> DriverResult<()> {
    device_res::devm_request_irq_with_provider(resource_provider(), device, irq, handler)
        .map_err(map_res_err)
}

/// Register primary and threaded interrupt handlers and tie their lifetime to
/// `device`.
///
/// The request is routed through the framework-owned `device_res` IRQ provider.
pub(crate) fn devm_request_threaded_irq(
    device: &DeviceObject,
    irq: IrqResource,
    primary: Arc<dyn DevIrqHandler>,
    thread: Arc<dyn DevIrqThreadHandler>,
    name: Option<&'static str>,
) -> DriverResult<()> {
    device_res::devm_request_threaded_irq_with_provider(
        resource_provider(),
        device,
        irq,
        primary,
        thread,
        name,
    )
    .map_err(map_res_err)
}

/// Register a default-primary threaded interrupt handler and tie its lifetime
/// to `device`.
///
/// The request is routed through the framework-owned `device_res` IRQ provider.
pub(crate) fn devm_request_threaded_irq_default(
    device: &DeviceObject,
    irq: IrqResource,
    thread: Arc<dyn DevIrqThreadHandler>,
    name: Option<&'static str>,
) -> DriverResult<()> {
    device_res::devm_request_threaded_irq_default_with_provider(
        resource_provider(),
        device,
        irq,
        thread,
        name,
    )
    .map_err(map_res_err)
}

/// Allocate a coherent DMA buffer and tie its lifetime to `device`.
///
/// The buffer is freed when the device's probe fails or the device is removed.
/// Returns the CPU virtual address and device bus address of the buffer.
pub(crate) fn devm_alloc_coherent(
    device: &DeviceObject,
    spec: DmaSpec,
) -> DriverResult<(NonNull<u8>, u64)> {
    device_res::devm_alloc_coherent_with_provider(resource_provider(), device, spec)
        .map_err(map_res_err)
}
/// Resolve a clock for `device` and tie the consumer handle's lifetime to it.
///
/// `spec` is normally built by the platform's firmware layer from the device's
/// DT `clocks` specifier (or by the SoC clock name). The underlying kclk
/// adapter lives in this crate's clock module.
pub fn devm_clk_get(
    device: &DeviceObject,
    spec: device_res::ClkSpec,
) -> DriverResult<device_res::Clk> {
    let clk = device_res::Clk::get_with(clk_op_provider(), spec).map_err(map_res_err)?;
    let cleanup = clk.clone();
    use device_res::DeviceResource as _;
    device.register_cleanup(Box::new(move || drop(cleanup)));
    Ok(clk)
}

/// The host `ClkOp` implementation resolving clocks through `kclk`.
fn clk_op_provider() -> &'static dyn device_res::ClkOp {
    &KCLK_OP
}

static KCLK_OP: KClkOp = KClkOp;

/// `ClkOp` adapter delegating to the kclk provider registry.
struct KClkOp;

impl device_res::ClkOp for KClkOp {
    fn get_clock(
        &self,
        spec: device_res::ClkSpec,
    ) -> ResResult<alloc::sync::Arc<dyn device_res::ClkConsumer>> {
        use alloc::sync::Arc;

        use kclk::ClkHandle;

        struct KclkConsumer(ClkHandle);
        impl device_res::ClkConsumer for KclkConsumer {
            fn name(&self) -> &str {
                self.0.name()
            }

            fn get_rate(&self) -> u64 {
                self.0.get_rate()
            }

            fn prepare(&self) -> ResResult<()> {
                self.0.prepare().map_err(map_clk_err)
            }

            fn unprepare(&self) {
                let _ = self.0.unprepare();
            }

            fn enable(&self) -> ResResult<()> {
                self.0.enable().map_err(map_clk_err)
            }

            fn disable(&self) {
                self.0.disable()
            }

            fn prepare_enable(&self) -> ResResult<()> {
                self.0.prepare_enable().map_err(map_clk_err)
            }

            fn disable_unprepare(&self) {
                self.0.disable_unprepare()
            }

            fn is_enabled(&self) -> ResResult<bool> {
                self.0.is_enabled().map_err(map_clk_err)
            }

            fn is_prepared(&self) -> ResResult<bool> {
                self.0.is_prepared().map_err(map_clk_err)
            }

            fn get_parent(&self) -> Option<Arc<dyn device_res::ClkConsumer>> {
                self.0
                    .get_parent()
                    .map(|p| Arc::new(KclkConsumer(p)) as Arc<dyn device_res::ClkConsumer>)
            }

            fn round_rate(&self, rate: u64) -> ResResult<u64> {
                self.0.round_rate(rate).map_err(map_clk_err)
            }

            fn set_rate(&self, rate: u64) -> ResResult<u64> {
                self.0.set_rate(rate).map_err(map_clk_err)
            }

            fn set_rate_range(&self, min: u64, max: u64) -> ResResult<()> {
                self.0.set_rate_range(min, max).map_err(map_clk_err)
            }

            fn get_rate_range(&self) -> (u64, u64) {
                self.0.get_rate_range()
            }

            fn set_parent(&self, index: u8) -> ResResult<()> {
                self.0.set_parent(index).map_err(map_clk_err)
            }

            fn register_notifier(&self, cb: device_res::ClkNotifier) -> ResResult<u64> {
                let kcb: kclk::ClkNotifier = Arc::new(move |event| {
                    let mapped = match *event {
                        kclk::ClkEvent::PreRateChange { old, new } => {
                            device_res::ClkRateEvent::PreRateChange { old, new }
                        }
                        kclk::ClkEvent::PostRateChange { old, new } => {
                            device_res::ClkRateEvent::PostRateChange { old, new }
                        }
                        kclk::ClkEvent::RateChangeAborted { old, attempted } => {
                            device_res::ClkRateEvent::RateChangeAborted { old, attempted }
                        }
                    };
                    match cb(&mapped) {
                        device_res::NotifierVote::Proceed => kclk::NotifierResult::Proceed,
                        device_res::NotifierVote::Stop => kclk::NotifierResult::Stop,
                    }
                });
                Ok(self.0.register_notifier(kcb))
            }

            fn unregister_notifier(&self, id: u64) -> ResResult<()> {
                self.0.unregister_notifier(id);
                Ok(())
            }
        }

        /// Translate a kclk error into the resource vocabulary.
        fn map_clk_err(err: kclk::ClkError) -> ResError {
            match err {
                kclk::ClkError::NoProvider | kclk::ClkError::NotFound => ResError::NotFound,
                kclk::ClkError::NotSupported => ResError::Unsupported,
                kclk::ClkError::InvalidRate | kclk::ClkError::InvalidParent => {
                    ResError::InvalidResource
                }
                kclk::ClkError::Busy | kclk::ClkError::Aborted => ResError::Busy,
                kclk::ClkError::Io | kclk::ClkError::ParentError | kclk::ClkError::BadState => {
                    ResError::InvalidResource
                }
            }
        }

        let handle = match spec.name() {
            Some(name) => kclk::clk_get_by_name(name).map_err(map_clk_err)?,
            None => kclk::clk_get_by_index(spec.provider(), spec.index()).map_err(map_clk_err)?,
        };
        Ok(Arc::new(KclkConsumer(handle)))
    }
}

/// Driver-facing device-managed resource methods.
///
/// Drivers use this extension trait to acquire resources from their
/// [`DeviceObject`] without naming the concrete host provider. The provider is
/// owned by the driver framework and passed explicitly into `device_res`.
pub trait DeviceResourceExt {
    /// Map an MMIO region and tie its lifetime to this device.
    fn devm_iomap(&self, region: MmioRegion, name: &'static str) -> DriverResult<NonNull<u8>>;

    /// Register a context-carrying interrupt handler and tie its lifetime to
    /// this device.
    fn devm_request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn DevIrqHandler>,
    ) -> DriverResult<()>;

    /// Register primary and threaded interrupt handlers and tie their lifetime
    /// to this device.
    fn devm_request_threaded_irq(
        &self,
        irq: IrqResource,
        primary: Arc<dyn DevIrqHandler>,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()>;

    /// Register a default-primary threaded interrupt handler and tie its
    /// lifetime to this device.
    fn devm_request_threaded_irq_default(
        &self,
        irq: IrqResource,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()>;

    /// Allocate a coherent DMA buffer and tie its lifetime to this device.
    fn devm_alloc_coherent(&self, spec: DmaSpec) -> DriverResult<(NonNull<u8>, u64)>;
}

impl DeviceResourceExt for DeviceObject {
    fn devm_iomap(&self, region: MmioRegion, name: &'static str) -> DriverResult<NonNull<u8>> {
        devm_iomap(self, region, name)
    }

    fn devm_request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn DevIrqHandler>,
    ) -> DriverResult<()> {
        devm_request_irq(self, irq, handler)
    }

    fn devm_request_threaded_irq(
        &self,
        irq: IrqResource,
        primary: Arc<dyn DevIrqHandler>,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()> {
        devm_request_threaded_irq(self, irq, primary, thread, name)
    }

    fn devm_request_threaded_irq_default(
        &self,
        irq: IrqResource,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()> {
        devm_request_threaded_irq_default(self, irq, thread, name)
    }

    fn devm_alloc_coherent(&self, spec: DmaSpec) -> DriverResult<(NonNull<u8>, u64)> {
        devm_alloc_coherent(self, spec)
    }
}
