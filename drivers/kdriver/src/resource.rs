// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver-facing resource helpers backed by the [`device_res`] provider model.
//!
//! The X-Kernel provider implementation lives in `device-res-xkernel`. This
//! module keeps the driver orchestration crate's `DriverResult` boundary while
//! acquiring resources through the OS-neutral `device_res` API.

use alloc::sync::Arc;
use core::ptr::NonNull;

use device_res::{
    DmaSpec, IrqHandler as DevIrqHandler, IrqResource, IrqThreadHandler as DevIrqThreadHandler,
    MmioRegion, ResError,
};
use device_res_xkernel::XKernelResourceProvider;
use driver_base::{DriverError, DriverResult};
use kdevice::DeviceObject;

static XKERNEL_RESOURCE_PROVIDER: XKernelResourceProvider = XKernelResourceProvider::new();

/// Install the X-Kernel resource provider backend.
///
/// Must run before any driver acquires a resource. It is installed before the
/// platform `early_driver_init` hook so that even early device interrupts flow
/// through the resource provider rather than `khal` directly.
pub fn install_resource_provider() {
    device_res::set_mmio_provider(&XKERNEL_RESOURCE_PROVIDER);
    device_res::set_dma_provider(&XKERNEL_RESOURCE_PROVIDER);
    device_res::set_irq_provider(&XKERNEL_RESOURCE_PROVIDER);
}

fn map_res_err(err: ResError) -> DriverError {
    match err {
        ResError::InvalidResource => DriverError::InvalidInput,
        ResError::MappingFailed => DriverError::Io,
        ResError::NoMemory => DriverError::NoMemory,
        ResError::Busy => DriverError::ResourceBusy,
        ResError::Unsupported => DriverError::Unsupported,
        ResError::NoProvider => DriverError::BadState,
    }
}

/// Map an MMIO region and tie its lifetime to `device`.
///
/// The mapping is released when the device's probe fails or the device is
/// removed. Returns the virtual base address for the region.
pub fn devm_iomap(
    device: &DeviceObject,
    region: MmioRegion,
    name: &'static str,
) -> DriverResult<NonNull<u8>> {
    device_res::devm_iomap(device, region, name).map_err(map_res_err)
}

/// Register a context-carrying interrupt handler and tie its lifetime to
/// `device`.
///
/// The handler is released when the device's probe fails or the device is
/// removed.
pub fn devm_request_irq(
    device: &DeviceObject,
    irq: IrqResource,
    handler: Arc<dyn DevIrqHandler>,
) -> DriverResult<()> {
    device_res::devm_request_irq(device, irq, handler).map_err(map_res_err)
}

/// Register primary and threaded interrupt handlers and tie their lifetime to
/// `device`.
///
/// The request is routed through the installed `device_res` IRQ provider.
pub fn devm_request_threaded_irq(
    device: &DeviceObject,
    irq: IrqResource,
    primary: Arc<dyn DevIrqHandler>,
    thread: Arc<dyn DevIrqThreadHandler>,
    name: Option<&'static str>,
) -> DriverResult<()> {
    device_res::devm_request_threaded_irq(device, irq, primary, thread, name).map_err(map_res_err)
}

/// Register a default-primary threaded interrupt handler and tie its lifetime
/// to `device`.
///
/// The request is routed through the installed `device_res` IRQ provider.
pub fn devm_request_threaded_irq_default(
    device: &DeviceObject,
    irq: IrqResource,
    thread: Arc<dyn DevIrqThreadHandler>,
    name: Option<&'static str>,
) -> DriverResult<()> {
    device_res::devm_request_threaded_irq_default(device, irq, thread, name).map_err(map_res_err)
}

/// Allocate a coherent DMA buffer and tie its lifetime to `device`.
///
/// The buffer is freed when the device's probe fails or the device is removed.
/// Returns the CPU virtual address and device bus address of the buffer.
pub fn devm_alloc_coherent(
    device: &DeviceObject,
    spec: DmaSpec,
) -> DriverResult<(NonNull<u8>, u64)> {
    device_res::devm_alloc_coherent(device, spec).map_err(map_res_err)
}
