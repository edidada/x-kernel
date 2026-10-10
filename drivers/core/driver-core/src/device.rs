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
    bus::{Bus, DeviceDescriptor, DeviceId},
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
}

// NOTE: there is deliberately no `WakeThread` variant. kirq has no
// threaded-handler vocabulary yet; a half-supported variant whose
// "wake the thread" half silently degrades to Handled at the runtime
// adapter would let drivers believe they run work in task context while
// actually running it in IRQ context. Re-add it together with a real
// threaded-IRQ path (DeviceServices::request_threaded_irq + a scheduler
// handoff), never as a silent degradation.

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

/// Type-erased interrupt handler registered by [`DeviceCtx::request_irq`].
///
/// The handler carries its own context (the `dev_id` pattern): drivers close
/// over a `Weak` handle into their own `Data`; there is no global irq-to-
/// device routing table anywhere in the framework.
pub type IrqHandlerFn = Arc<dyn Fn() -> IrqAction + Send + Sync>;

/// A family-specific service extension (virtio transport probing, future
/// controller abstractions). The family declares the trait and teaches it
/// how to recover itself from the installed runtime's service surface;
/// driver-core itself never names a device family.
pub trait ExtensionService: Sized {
    /// Recover the extension from the runtime's service object, if the
    /// installed runtime provides this capability.
    fn from_services(services: &dyn DeviceServices) -> Option<Self>;
}

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

    /// The runtime's concrete type, for extension-trait downcasts. The core
    /// stays family-agnostic: extensions name the runtime type they support.
    fn as_any(&self) -> &dyn core::any::Any;
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
    /// Construct an IoMap over an ALREADY-MAPPED region.
    ///
    /// # Safety
    ///
    /// `vaddr` must point at a valid mapping of at least `region.size`
    /// bytes for the lifetime of this handle, and the caller must own the
    /// mapping (pairing the eventual release). A safe constructor here
    /// would let any `usize` reach the volatile accessors — that is a
    /// soundness hole, not an inconvenience.
    pub unsafe fn new(vaddr: usize, region: MmioRegion, release: Option<ReleaseHook>) -> Self {
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
        // Alignment is part of the safe contract: `read_volatile::<u32>`
        // requires 4-byte alignment and "registers are naturally aligned"
        // comments cannot constrain a safe `usize` offset. Misalignment
        // panics here instead of being UB at the volatile op.
        let align = size.max(1);
        if !(self.vaddr + offset).is_multiple_of(align) {
            panic!(
                "MMIO access misaligned: vaddr {:#x} + offset {:#x} not {}-byte aligned",
                self.vaddr, offset, align
            );
        }
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
        // SAFETY: `access_ptr` bounds-checks the offset against the mapped
        // region and the pointer is aligned for u8 by construction.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 1)) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read16(&self, offset: usize) -> u16 {
        // SAFETY: bounds-checked by `access_ptr`; MMIO u16 registers are
        // naturally aligned, so the cast keeps alignment.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 2) as *const u16) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read32(&self, offset: usize) -> u32 {
        // SAFETY: bounds-checked by `access_ptr`; MMIO u32 registers are
        // naturally aligned, so the cast keeps alignment.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 4) as *const u32) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read64(&self, offset: usize) -> u64 {
        // SAFETY: bounds-checked by `access_ptr`; MMIO u64 registers are
        // naturally aligned, so the cast keeps alignment.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 8) as *const u64) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn write8(&self, offset: usize, value: u8) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; u8 needs no alignment.
        unsafe { ptr::write_volatile(self.access_ptr(offset, 1), value) };
    }

    #[inline]
    pub fn write16(&self, offset: usize, value: u16) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; naturally aligned for u16.
        unsafe { ptr::write_volatile(self.access_ptr(offset, 2) as *mut u16, value) };
    }

    #[inline]
    pub fn write32(&self, offset: usize, value: u32) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; naturally aligned for u32.
        unsafe { ptr::write_volatile(self.access_ptr(offset, 4) as *mut u32, value) };
    }

    #[inline]
    pub fn write64(&self, offset: usize, value: u64) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; naturally aligned for u64.
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
/// The `kclk` subsystem (the common clock framework core, ported from the `clk/tail` branch
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

    /// Prepare and enable, returning an ownership token for the ENABLED
    /// state. The token's Drop balances the prepare/enable pair — the
    /// review's finding was that `&self` + a bare `Ok(())` left probe-failure
    /// paths (request_irq/class publication errors AFTER a successful
    /// enable) with no rollback; remove-time cleanup relied on reading the
    /// provider's current state instead of local ownership.
    pub fn prepare_enable(&self) -> Result<EnabledClock, RuntimeError> {
        self.ops.prepare()?;
        if let Err(err) = self.ops.enable() {
            // Roll back the prepare: a failed enable must not leave the
            // clock half-prepared.
            let () = self.ops.disable();
            self.ops.unprepare();
            return Err(err);
        }
        Ok(EnabledClock {
            ops: Arc::clone(&self.ops),
        })
    }

    pub fn ops(&self) -> &dyn ClkOps {
        self.ops.as_ref()
    }
}

/// Ownership of one prepare+enable pair on a [`ClkHandle`].
///
/// Drop balances the pair (disable + unprepare). [`Self::keep_running`]
/// consumes the token WITHOUT balancing — for consumers that deliberately
/// hold the clock past device removal (the adopted stdout port's console
/// continuity policy). Every other path lets the token drop: probe failure
/// rolls back automatically, `remove` drops it from `Data`.
///
/// The token OWNS a clone of the handle's operations arc (not a borrow):
/// it is `'static` and can live in driver `Data`, so the balance is
/// structural — no `remove`-time "is it still enabled?" re-reading of
/// provider state.
pub struct EnabledClock {
    ops: Arc<dyn ClkOps>,
}

impl EnabledClock {
    /// Consume the token WITHOUT disabling: the caller takes explicit,
    /// permanent ownership of the enabled state. Use only when the clock
    /// must outlive the device binding (stdout console).
    pub fn keep_running(self) {
        core::mem::forget(self)
    }
}

impl Drop for EnabledClock {
    fn drop(&mut self) {
        self.ops.disable();
        self.ops.unprepare();
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

    pub const fn id(&self) -> DeviceId {
        DeviceId(self.id)
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

    /// Look up a family-specific service extension on the installed runtime
    /// (e.g. the virtio transport service). Extensions keep device-family
    /// knowledge OUT of driver-core: the core stays generic, families
    /// declare their own extension traits and ask for them here.
    pub fn extension<E: ExtensionService>(&self) -> Option<E> {
        E::from_services(self.services)
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

    pub const fn id(&self) -> DeviceId {
        DeviceId(self.id)
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
