// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global capability provider registry and device-managed (`devm_*`) helpers.
//!
//! The host kernel installs one [`MmioOp`], [`DmaOp`], [`IrqOp`], and
//! [`ClkOp`] implementation each during early init. Drivers acquire resources
//! either through the RAII handles ([`Io`], [`Irq`], [`DmaCoherent`], [`Clk`])
//! or the `devm_*` helpers that bind resource lifetime to a
//! [`DeviceResource`].

use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;

#[cfg(not(unittest))]
use klazy::Once;

use crate::{
    DeviceResource, ResError, ResResult,
    clk::{Clk, ClkOp, ClkSpec},
    dma::{DmaCoherent, DmaOp, DmaSpec},
    irq::{Irq, IrqHandler, IrqOp, IrqResource},
    mmio::{Io, MmioOp, MmioRegion},
};

// Capability provider registry.
//
// Production uses `klazy::Once` for lock-free reads (a single acquire load
// after installation). Under `--cfg unittest` the registry is backed by a
// replaceable `SpinLock<Option<…>>` instead, so unit tests can swap in a mock
// quadruple even though the host kernel has already installed the real
// providers during early init (which runs before the test harness).
#[cfg(not(unittest))]
static MMIO_PROVIDER: Once<&'static dyn MmioOp> = Once::new();
#[cfg(not(unittest))]
static DMA_PROVIDER: Once<&'static dyn DmaOp> = Once::new();
#[cfg(not(unittest))]
static IRQ_PROVIDER: Once<&'static dyn IrqOp> = Once::new();
#[cfg(not(unittest))]
static CLK_PROVIDER: Once<&'static dyn ClkOp> = Once::new();

#[cfg(unittest)]
static MMIO_PROVIDER: kspin::SpinNoIrq<Option<&'static dyn MmioOp>> = kspin::SpinNoIrq::new(None);
#[cfg(unittest)]
static DMA_PROVIDER: kspin::SpinNoIrq<Option<&'static dyn DmaOp>> = kspin::SpinNoIrq::new(None);
#[cfg(unittest)]
static IRQ_PROVIDER: kspin::SpinNoIrq<Option<&'static dyn IrqOp>> = kspin::SpinNoIrq::new(None);
#[cfg(unittest)]
static CLK_PROVIDER: kspin::SpinNoIrq<Option<&'static dyn ClkOp>> = kspin::SpinNoIrq::new(None);

/// Install the MMIO capability provider. Must be called exactly once during
/// early kernel init, before any driver acquires a resource.
#[cfg(not(unittest))]
pub fn set_mmio_provider(p: &'static dyn MmioOp) {
    MMIO_PROVIDER.call_once(|| p);
}
#[cfg(not(unittest))]
pub fn set_dma_provider(p: &'static dyn DmaOp) {
    DMA_PROVIDER.call_once(|| p);
}
#[cfg(not(unittest))]
pub fn set_irq_provider(p: &'static dyn IrqOp) {
    IRQ_PROVIDER.call_once(|| p);
}
/// Install the clock capability provider. Must be called exactly once during
/// early kernel init, before any driver resolves a clock.
#[cfg(not(unittest))]
pub fn set_clk_provider(p: &'static dyn ClkOp) {
    CLK_PROVIDER.call_once(|| p);
}

#[cfg(unittest)]
pub fn set_mmio_provider(p: &'static dyn MmioOp) {
    *MMIO_PROVIDER.lock() = Some(p);
}
#[cfg(unittest)]
pub fn set_dma_provider(p: &'static dyn DmaOp) {
    *DMA_PROVIDER.lock() = Some(p);
}
#[cfg(unittest)]
pub fn set_irq_provider(p: &'static dyn IrqOp) {
    *IRQ_PROVIDER.lock() = Some(p);
}
#[cfg(unittest)]
pub fn set_clk_provider(p: &'static dyn ClkOp) {
    *CLK_PROVIDER.lock() = Some(p);
}

/// Returns `true` once all four providers have been installed.
pub fn provider_installed() -> bool {
    try_mmio_provider().is_some()
        && try_dma_provider().is_some()
        && try_irq_provider().is_some()
        && try_clk_provider().is_some()
}

/// Returns the installed MMIO provider, or `None` if unset.
#[cfg(not(unittest))]
pub fn try_mmio_provider() -> Option<&'static dyn MmioOp> {
    MMIO_PROVIDER.get().copied()
}
#[cfg(unittest)]
pub fn try_mmio_provider() -> Option<&'static dyn MmioOp> {
    *MMIO_PROVIDER.lock()
}

/// Returns the installed DMA provider, or `None` if unset.
#[cfg(not(unittest))]
pub fn try_dma_provider() -> Option<&'static dyn DmaOp> {
    DMA_PROVIDER.get().copied()
}
#[cfg(unittest)]
pub fn try_dma_provider() -> Option<&'static dyn DmaOp> {
    *DMA_PROVIDER.lock()
}

/// Returns the installed IRQ provider, or `None` if unset.
#[cfg(not(unittest))]
pub fn try_irq_provider() -> Option<&'static dyn IrqOp> {
    IRQ_PROVIDER.get().copied()
}
#[cfg(unittest)]
pub fn try_irq_provider() -> Option<&'static dyn IrqOp> {
    *IRQ_PROVIDER.lock()
}

/// Returns the installed clock provider, or `None` if unset.
#[cfg(not(unittest))]
pub fn try_clk_provider() -> Option<&'static dyn ClkOp> {
    CLK_PROVIDER.get().copied()
}
#[cfg(unittest)]
pub fn try_clk_provider() -> Option<&'static dyn ClkOp> {
    *CLK_PROVIDER.lock()
}

/// Uninstall all four providers.
///
/// Only available under `--cfg unittest`, where the registry is replaceable;
/// used to exercise the [`NoProvider`](ResError::NoProvider) error path before
/// a test re-installs a mock quadruple.
#[cfg(unittest)]
pub fn reset_providers() {
    *MMIO_PROVIDER.lock() = None;
    *DMA_PROVIDER.lock() = None;
    *IRQ_PROVIDER.lock() = None;
    *CLK_PROVIDER.lock() = None;
}

/// Returns the installed MMIO provider, or [`NoProvider`](ResError::NoProvider).
pub fn mmio_provider() -> ResResult<&'static dyn MmioOp> {
    try_mmio_provider().ok_or(ResError::NoProvider)
}
/// Returns the installed DMA provider, or [`NoProvider`](ResError::NoProvider).
pub fn dma_provider() -> ResResult<&'static dyn DmaOp> {
    try_dma_provider().ok_or(ResError::NoProvider)
}
/// Returns the installed IRQ provider, or [`NoProvider`](ResError::NoProvider).
pub fn irq_provider() -> ResResult<&'static dyn IrqOp> {
    try_irq_provider().ok_or(ResError::NoProvider)
}
/// Returns the installed clock provider, or [`NoProvider`](ResError::NoProvider).
pub fn clk_provider() -> ResResult<&'static dyn ClkOp> {
    try_clk_provider().ok_or(ResError::NoProvider)
}

/// Map an MMIO region and tie its lifetime to `device`.
///
/// The mapping is released when the device's probe fails or it is removed.
/// Returns the virtual base pointer directly (an [`Io`] handle would outlive
/// the function scope). Use [`Io::map`] when manual lifetime control is needed.
///
/// # Errors
///
/// See [`Io::map`].
pub fn devm_iomap(
    device: &dyn DeviceResource,
    region: MmioRegion,
    name: &'static str,
) -> ResResult<NonNull<u8>> {
    let io = Io::map(region, name)?;
    let ptr = io.as_ptr();
    device.register_cleanup(Box::new(move || drop(io)));
    Ok(ptr)
}

/// Register an interrupt handler and tie its lifetime to `device`.
///
/// # Errors
///
/// See [`Irq::request`].
pub fn devm_request_irq(
    device: &dyn DeviceResource,
    irq: IrqResource,
    handler: Arc<dyn IrqHandler>,
) -> ResResult<()> {
    let guard = Irq::request(irq, handler)?;
    device.register_cleanup(Box::new(move || drop(guard)));
    Ok(())
}

/// Allocate a coherent DMA buffer and tie its lifetime to `device`.
///
/// Returns the CPU virtual address and device bus address.
///
/// # Errors
///
/// See [`DmaCoherent::alloc`].
pub fn devm_alloc_coherent(
    device: &dyn DeviceResource,
    spec: DmaSpec,
) -> ResResult<(NonNull<u8>, u64)> {
    let dma = DmaCoherent::alloc(spec)?;
    let cpu_ptr = dma.cpu_ptr();
    let bus_addr = dma.bus_addr();
    device.register_cleanup(Box::new(move || drop(dma)));
    Ok((cpu_ptr, bus_addr))
}

/// Resolve a clock specifier and tie its lifetime to `device`.
///
/// The returned [`Clk`] is cloneable; the device-managed clone is released when
/// the device's probe fails or it is removed, and any clones the driver still
/// holds follow normal reference-counting.
///
/// # Errors
///
/// See [`Clk::get`].
pub fn devm_clk_get(device: &dyn DeviceResource, spec: ClkSpec) -> ResResult<Clk> {
    let clk = Clk::get(spec)?;
    let cleanup = clk.clone();
    device.register_cleanup(Box::new(move || drop(cleanup)));
    Ok(clk)
}
