// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The probe-side surface: per-device services and the guards they return.
//!
//! Drivers never see a provider, an irq-number routing table, or a cleanup
//! closure list. They call methods on [`DeviceCtx`] and store the returned
//! guards in their [`DriverOps::Data`](crate::DriverOps::Data):
//!
//! ```ignore
//! fn probe(&self, dev: &mut DeviceCtx<'_, PlatformBus>) -> Result<SerData, ProbeError> {
//!     let regs = dev.iomap_named("regs")?;            // devm_ioremap_resource
//!     let baud = dev.clk("baudclk")?;                 // clk_get + EPROBE_DEFER
//!     baud.prepare_enable()?;
//!     let irq = dev.request_irq_by_name("rx", move || {
//!         /* captured Weak<Data>, the dev_id pattern */
//!         IrqAction::Handled
//!     })?;
//!     Ok(SerData { regs, baud, irq /*, ... */ })      // ownership = lifetime
//! }
//! ```
//!
//! Field drop order in `Data` is the release order: declare interrupt guards
//! first so they are released before mappings disappear. The framework also
//! invokes [`DriverOps::remove`] before dropping `Data`, giving drivers a
//! quiesce window.

use alloc::sync::Arc;
use core::{
    fmt,
    marker::PhantomData,
    ptr,
    sync::atomic::{Ordering, fence},
};

use crate::{
    bus::{Bus, DeviceDescriptor},
    error::{DepSource, ProbeError, RuntimeError},
    resource::{ClkSpec, IrqSpec, MmioRegion},
};

/// Deferred resource release hook carried by every guard.
pub type ReleaseHook = Arc<dyn Fn() + Send + Sync>;

/// Outcome of an interrupt handler invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqAction {
    /// The interrupt was ours and has been acked.
    Handled,
    /// Not ours (shared line).
    NotHandled,
    /// Handled; additionally wake the threaded part.
    WakeThread,
}

/// Service errors, including deferred dependencies.
///
/// Unlike [`ProbeError`], a service can report that *one* acquisition is
/// deferred while others might still succeed; [`DeviceCtx`] folds
/// [`ServiceError::Deferred`] into [`ProbeError::Deferred`] for the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceError {
    Deferred(DepSource),
    Fatal(RuntimeError),
}

impl From<ServiceError> for ProbeError {
    fn from(err: ServiceError) -> Self {
        match err {
            ServiceError::Deferred(dep) => ProbeError::Deferred(dep),
            ServiceError::Fatal(err) => ProbeError::Fatal(err),
        }
    }
}

impl From<RuntimeError> for ServiceError {
    fn from(err: RuntimeError) -> Self {
        Self::Fatal(err)
    }
}

/// Opaque virtio transport probed by the service layer. The concrete
/// backing (PCI or MMIO) is a runtime implementation detail; drivers
/// downcast via [`VirtioTransport::downcast`] with the type their device
/// constructor expects, or consume it through
/// [`DeviceCtx::virtio_transport_into`].
pub struct VirtioTransport {
    inner: alloc::boxed::Box<dyn core::any::Any + Send + Sync>,
}

impl VirtioTransport {
    pub fn new(inner: alloc::boxed::Box<dyn core::any::Any + Send + Sync>) -> Self {
        Self { inner }
    }

    /// Downcast to the concrete transport type.
    pub fn downcast<T: Send + Sync + 'static>(self) -> Option<T> {
        self.inner.downcast::<T>().ok().map(|boxed| *boxed)
    }
}

/// Type-erased interrupt handler registered by [`DeviceCtx::request_irq`].
///
/// The handler carries its own context (the `dev_id` pattern): drivers close
/// over a `Weak` handle into their own `Data`; there is no global irq-to-
/// device routing table anywhere in the framework.
pub type IrqHandlerFn = Arc<dyn Fn() -> IrqAction + Send + Sync>;

/// The per-device service surface, implemented by the runtime layer.
///
/// This is a framework-internal seam: drivers touch it only through
/// [`DeviceCtx`]. It exists so the runtime (and test doubles) can be swapped
/// without the model depending on any host implementation.
pub trait DeviceServices: Send + Sync {
    /// Map an MMIO region for the device (`devm_ioremap`).
    fn iomap(
        &self,
        device: u64,
        region: MmioRegion,
        name: &'static str,
    ) -> Result<IoMap, RuntimeError>;

    /// Register an interrupt for the device (`request_irq` with dev_id
    /// captured in the handler).
    fn request_irq(
        &self,
        device: u64,
        spec: IrqSpec,
        name: &'static str,
        handler: IrqHandlerFn,
    ) -> Result<DevIrq, RuntimeError>;

    /// Allocate coherent DMA memory (`dmam_alloc_coherent`).
    fn alloc_coherent(
        &self,
        device: u64,
        len: usize,
        align: usize,
    ) -> Result<CoherentMem, RuntimeError>;

    /// Acquire a clock input (`clk_get`); may report that the provider has
    /// not registered yet.
    fn clk_get(&self, device: u64, spec: ClkSpec) -> Result<ClkHandle, ServiceError>;

    /// Probe the virtio transport for a PCI device: configures the function,
    /// builds the transport, and resolves its irq. Drivers receive the
    /// kind + transport + irq without touching PCI internals or providers.
    fn virtio_pci_transport(
        &self,
        device: u64,
        location: crate::desc::DeviceLocation,
    ) -> Result<(crate::DeviceKind, crate::VirtioTransport, Option<usize>), RuntimeError>;
}

/// A mapped MMIO region with checked volatile accessors.
///
/// Auto-unmapped on drop. The accessors carry release/acquire fences and
/// bounds checks, matching readl/writel semantics.
pub struct IoMap {
    vaddr: usize,
    region: MmioRegion,
    release: Option<ReleaseHook>,
}

impl fmt::Debug for IoMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoMap")
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl IoMap {
    /// Constructed by [`DeviceServices`] implementations.
    pub fn new(vaddr: usize, region: MmioRegion, release: Option<ReleaseHook>) -> Self {
        Self {
            vaddr,
            region,
            release,
        }
    }

    pub const fn region(&self) -> MmioRegion {
        self.region
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.vaddr as *mut u8
    }

    #[inline]
    fn access_ptr(&self, offset: usize, size: usize) -> *mut u8 {
        let end = offset
            .checked_add(size)
            .expect("MMIO access offset overflow");
        assert!(end <= self.region.size, "MMIO access out of bounds");
        // SAFETY: the mapping covers `region.size` bytes and the checked
        // offset is within it.
        unsafe { self.as_ptr().add(offset) }
    }

    #[inline]
    pub fn read8(&self, offset: usize) -> u8 {
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 1)) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read16(&self, offset: usize) -> u16 {
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 2) as *const u16) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read32(&self, offset: usize) -> u32 {
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 4) as *const u32) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read64(&self, offset: usize) -> u64 {
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 8) as *const u64) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn write8(&self, offset: usize, value: u8) {
        fence(Ordering::Release);
        unsafe { ptr::write_volatile(self.access_ptr(offset, 1), value) };
    }

    #[inline]
    pub fn write16(&self, offset: usize, value: u16) {
        fence(Ordering::Release);
        unsafe { ptr::write_volatile(self.access_ptr(offset, 2) as *mut u16, value) };
    }

    #[inline]
    pub fn write32(&self, offset: usize, value: u32) {
        fence(Ordering::Release);
        unsafe { ptr::write_volatile(self.access_ptr(offset, 4) as *mut u32, value) };
    }

    #[inline]
    pub fn write64(&self, offset: usize, value: u64) {
        fence(Ordering::Release);
        unsafe { ptr::write_volatile(self.access_ptr(offset, 8) as *mut u64, value) };
    }
}

impl Drop for IoMap {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// A registered device interrupt; released (handler freed, line freed) on
/// drop.
pub struct DevIrq {
    number: usize,
    release: Option<ReleaseHook>,
}

impl fmt::Debug for DevIrq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DevIrq")
            .field("number", &self.number)
            .finish_non_exhaustive()
    }
}

impl DevIrq {
    /// Constructed by [`DeviceServices`] implementations.
    pub fn new(number: usize, release: Option<ReleaseHook>) -> Self {
        Self { number, release }
    }

    pub const fn number(&self) -> usize {
        self.number
    }
}

impl Drop for DevIrq {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Coherent DMA memory; freed on drop.
pub struct CoherentMem {
    cpu: usize,
    bus: u64,
    len: usize,
    release: Option<ReleaseHook>,
}

impl fmt::Debug for CoherentMem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoherentMem")
            .field("bus", &self.bus)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl CoherentMem {
    /// Constructed by [`DeviceServices`] implementations.
    pub fn new(cpu: usize, bus: u64, len: usize, release: Option<ReleaseHook>) -> Self {
        Self {
            cpu,
            bus,
            len,
            release,
        }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.cpu as *mut u8
    }

    pub const fn bus_addr(&self) -> u64 {
        self.bus
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for CoherentMem {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Provider-side clock operations behind a [`ClkHandle`].
///
/// The `kclk` subsystem (ported from the `clk/tail` branch during Phase 0
/// acceptance) implements this for real hardware clocks; tests implement it
/// with fixed-rate fakes.
pub trait ClkOps: Send + Sync {
    fn name(&self) -> &str;

    fn get_rate(&self) -> u64;

    fn round_rate(&self, rate: u64) -> Result<u64, RuntimeError>;

    fn set_rate(&self, rate: u64) -> Result<u64, RuntimeError>;

    fn prepare(&self) -> Result<(), RuntimeError>;

    fn unprepare(&self);

    fn enable(&self) -> Result<(), RuntimeError>;

    fn disable(&self);

    fn is_prepared(&self) -> bool;

    fn is_enabled(&self) -> bool;
}

/// An acquired clock input (`struct clk *`); drops the reference on drop.
///
/// `prepare`/`enable` are not undone automatically: quiesce belongs to
/// `remove()`, mirroring Linux ownership rules.
#[derive(Clone)]
pub struct ClkHandle {
    ops: Arc<dyn ClkOps>,
    release: Option<ReleaseHook>,
}

impl ClkHandle {
    /// Constructed by [`DeviceServices`] implementations.
    pub fn new(ops: Arc<dyn ClkOps>, release: Option<ReleaseHook>) -> Self {
        Self { ops, release }
    }

    pub fn prepare_enable(&self) -> Result<(), RuntimeError> {
        self.ops.prepare()?;
        self.ops.enable()
    }

    pub fn disable_unprepare(&self) {
        self.ops.disable();
        self.ops.unprepare();
    }

    pub fn ops(&self) -> &dyn ClkOps {
        self.ops.as_ref()
    }
}

impl core::ops::Deref for ClkHandle {
    type Target = dyn ClkOps;

    fn deref(&self) -> &Self::Target {
        self.ops.as_ref()
    }
}

impl Drop for ClkHandle {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

impl fmt::Debug for ClkHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClkHandle")
            .field("name", &self.ops.name())
            .field("rate", &self.ops.get_rate())
            .finish_non_exhaustive()
    }
}

/// Probe-time handle to one device (`struct device *` during probe).
///
/// Every method either succeeds, fails fatally, or — for dependency-backed
/// resources — returns [`ProbeError::Deferred`].
pub struct DeviceCtx<'a, B: Bus> {
    descriptor: &'a DeviceDescriptor,
    id: u64,
    modalias: Option<&'a str>,
    services: &'a dyn DeviceServices,
    _bus: PhantomData<fn(&B)>,
}

impl<'a, B: Bus> DeviceCtx<'a, B> {
    /// Constructed by the domain when it probes a device.
    pub fn new(
        descriptor: &'a DeviceDescriptor,
        id: u64,
        services: &'a dyn DeviceServices,
    ) -> Self {
        Self {
            descriptor,
            id,
            modalias: None,
            services,
            _bus: PhantomData,
        }
    }

    /// Constructed by the domain with the matched id row's `MODALIAS`.
    pub fn with_modalias(
        descriptor: &'a DeviceDescriptor,
        id: u64,
        modalias: Option<&'a str>,
        services: &'a dyn DeviceServices,
    ) -> Self {
        Self {
            descriptor,
            id,
            modalias,
            services,
            _bus: PhantomData,
        }
    }

    /// The `MODALIAS` derived from the id-table row this device matched
    /// (published through uevent so userland can load drivers by alias).
    pub fn modalias(&self) -> Option<&str> {
        self.modalias
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub const fn descriptor(&self) -> &'a DeviceDescriptor {
        self.descriptor
    }

    pub fn resources(&self) -> &crate::resource::ResourceMap {
        &self.descriptor.resources
    }

    /// `platform_get_resource(pdev, IORESOURCE_MEM, n)`.
    pub fn mmio(&self, index: usize) -> Result<MmioRegion, ProbeError> {
        self.resources()
            .mmio(index)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))
    }

    /// `platform_get_resource_byname(pdev, IORESOURCE_MEM, name)`.
    pub fn mmio_by_name(&self, name: &str) -> Result<MmioRegion, ProbeError> {
        self.resources()
            .mmio_by_name(name)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))
    }

    /// `platform_get_irq(pdev, n)`.
    pub fn irq(&self, index: usize) -> Result<IrqSpec, ProbeError> {
        self.resources()
            .irq(index)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))
    }

    /// `platform_get_irq_byname(pdev, name)`.
    pub fn irq_by_name(&self, name: &str) -> Result<IrqSpec, ProbeError> {
        self.resources()
            .irq_by_name(name)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))
    }

    /// `devm_ioremap_resource(dev, res)` on the n-th MMIO region.
    pub fn iomap(&mut self, index: usize) -> Result<IoMap, ProbeError> {
        let region = self.mmio(index)?;
        self.iomap_region(region, "mmio")
    }

    /// `devm_platform_ioremap_resource(pdev, index)` variant that resolves
    /// `reg-names` first.
    pub fn iomap_named(&mut self, name: &str) -> Result<IoMap, ProbeError> {
        let region = self.mmio_by_name(name)?;
        self.iomap_region(region, "mmio")
    }

    /// Map an explicit region (used by PCI BARs resolved by the bus).
    pub fn iomap_region(
        &mut self,
        region: MmioRegion,
        name: &'static str,
    ) -> Result<IoMap, ProbeError> {
        self.services
            .iomap(self.id, region, name)
            .map_err(ProbeError::Fatal)
    }

    /// `devm_request_irq(dev, irq, handler, 0, name, dev_id)` — the handler
    /// closes over its own context (`dev_id`), typically a `Weak` into the
    /// driver's `Data`.
    pub fn request_irq<F>(
        &mut self,
        spec: IrqSpec,
        name: &'static str,
        handler: F,
    ) -> Result<DevIrq, ProbeError>
    where
        F: Fn() -> IrqAction + Send + Sync + 'static,
    {
        self.services
            .request_irq(self.id, spec, name, Arc::new(handler))
            .map_err(ProbeError::Fatal)
    }

    /// [`DeviceCtx::request_irq`] on the n-th interrupt resource.
    pub fn request_irq_at<F>(&mut self, index: usize, handler: F) -> Result<DevIrq, ProbeError>
    where
        F: Fn() -> IrqAction + Send + Sync + 'static,
    {
        let spec = self.irq(index)?;
        self.request_irq(spec, "irq", handler)
    }

    /// [`DeviceCtx::request_irq`] on an `interrupt-names` entry.
    pub fn request_irq_by_name<F>(&mut self, name: &str, handler: F) -> Result<DevIrq, ProbeError>
    where
        F: Fn() -> IrqAction + Send + Sync + 'static,
    {
        let spec = self.irq_by_name(name)?;
        self.request_irq(spec, "irq", handler)
    }

    /// `dmam_alloc_coherent(dev, size, dma_handle, gfp)`.
    pub fn alloc_coherent(&mut self, len: usize, align: usize) -> Result<CoherentMem, ProbeError> {
        self.services
            .alloc_coherent(self.id, len, align)
            .map_err(ProbeError::Fatal)
    }

    /// `clk_get(dev, id)`: resolves a `clock-names` entry and acquires the
    /// clock; returns [`ProbeError::Deferred`] while the provider is missing.
    pub fn clk(&mut self, name: &str) -> Result<ClkHandle, ProbeError> {
        let spec = self
            .resources()
            .clk_by_name(name)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        self.clk_spec(spec)
    }

    /// [`DeviceCtx::clk`] on an explicit spec.
    pub fn clk_spec(&mut self, spec: ClkSpec) -> Result<ClkHandle, ProbeError> {
        self.services
            .clk_get(self.id, spec)
            .map_err(ProbeError::from)
    }

    /// Build a `-EPROBE_DEFER` return value for an out-of-band dependency.
    pub fn defer(&self, dep: DepSource) -> ProbeError {
        ProbeError::Deferred(dep)
    }

    /// Probe this device's virtio transport (PCI): returns the device kind,
    /// the type-erased transport, and the resolved irq. The framework does
    /// PCI configuration and provider access; the driver does neither.
    pub fn virtio_pci_transport(
        &mut self,
    ) -> Result<(crate::DeviceKind, crate::VirtioTransport, Option<usize>), ProbeError> {
        let location = self.descriptor().location;
        self.services
            .virtio_pci_transport(self.id, location)
            .map_err(ProbeError::Fatal)
    }

    /// [`DeviceCtx::virtio_pci_transport`] with the transport downcast to
    /// the concrete type the device constructor takes.
    pub fn virtio_pci_transport_as<T: Send + Sync + 'static>(
        &mut self,
    ) -> Result<(crate::DeviceKind, T, Option<usize>), ProbeError> {
        let (kind, transport, irq) = self.virtio_pci_transport()?;
        let transport = transport
            .downcast::<T>()
            .ok_or(ProbeError::Fatal(RuntimeError::BadState))?;
        Ok((kind, transport, irq))
    }
}

/// Remove-time handle to one bound device: descriptor plus the driver's own
/// data, with services available for last-minute releases.
///
/// Guards stored in `Data` are released *after* `remove` returns, so drivers
/// quiesce here (stop queues, mask interrupts at the device, drain work)
/// exactly as they do in Linux.
pub struct BoundDevice<'a, B: Bus, D> {
    descriptor: &'a DeviceDescriptor,
    id: u64,
    data: &'a mut D,
    services: &'a dyn DeviceServices,
    _bus: PhantomData<fn(&B)>,
}

impl<'a, B: Bus, D> BoundDevice<'a, B, D> {
    /// Constructed by the domain when it removes a device.
    pub fn new(
        descriptor: &'a DeviceDescriptor,
        id: u64,
        data: &'a mut D,
        services: &'a dyn DeviceServices,
    ) -> Self {
        Self {
            descriptor,
            id,
            data,
            services,
            _bus: PhantomData,
        }
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub const fn descriptor(&self) -> &'a DeviceDescriptor {
        self.descriptor
    }

    pub const fn data(&self) -> &D {
        self.data
    }

    pub fn data_mut(&mut self) -> &mut D {
        self.data
    }

    pub fn services(&self) -> &'a dyn DeviceServices {
        self.services
    }
}
