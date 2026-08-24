// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel provider adapter and device-managed helpers for [`device_res`].
//!
//! The OS-agnostic resource model in [`device_res`] describes *what* a driver
//! needs; this crate binds those operations to X-Kernel's MMIO mapping, IRQ
//! manager, and coherent DMA allocator, and installs the backend during driver
//! init. For IRQs, this crate is the adapter between `device_res` vocabulary
//! and `kirq`; the IRQ core does not depend on devres.
//!
//! The `devm_*` helpers acquire a resource and tie its lifetime to a
//! [`DeviceObject`] via its devres cleanup list, so a failed probe or a later
//! removal releases the resource automatically and in reverse acquisition
//! order.

#![no_std]

extern crate alloc;

mod dma;
mod irq;
mod mmio;

use alloc::sync::Arc;
use core::ptr::NonNull;

use device_res::{
    DmaSpec, IrqHandler as DevIrqHandler, IrqResource, IrqThreadHandler as DevIrqThreadHandler,
    MmioRegion, ResError,
};
use driver_base::{DriverError, DriverResult};
use kdevice::DeviceObject;

/// X-Kernel implementation of the OS-agnostic resource provider.
pub(crate) struct XKernelResourceProvider;

static XKERNEL_PROVIDER: XKernelResourceProvider = XKernelResourceProvider;

/// Install the X-Kernel resource provider backend.
///
/// Must run before any driver acquires a resource. It is installed before the
/// platform `early_driver_init` hook so that even early device interrupts (such
/// as the console input line) flow through the resource provider rather than
/// `khal` directly.
pub fn install_resource_provider() {
    device_res::set_mmio_provider(&XKERNEL_PROVIDER);
    device_res::set_dma_provider(&XKERNEL_PROVIDER);
    device_res::set_irq_provider(&XKERNEL_PROVIDER);
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
/// The primary handler runs in hardirq context. Return
/// [`device_res::IrqEvent::WAKE_THREAD`] from it to wake the sleepable threaded
/// handler.
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
/// The default primary handler wakes the thread and uses kirq generic ONESHOT
/// masking so level-triggered lines stay masked until the thread is idle.
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
