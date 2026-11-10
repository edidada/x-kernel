// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The device handle: `Device` — the one object a driver holds, in probe
//! and in remove (Linux `struct device *`).

use alloc::sync::Arc;
use core::marker::PhantomData;

use crate::{
    bus::Bus,
    devres::{
        ClkHandle, CoherentMem, DevIrq, IoMap, IrqAction, PhyHandle, ResetHandle,
        services::DeviceServices,
    },
    error::{DepSource, ProbeError, RuntimeError},
    extension::ExtensionService,
    firmware::{FirmwareNodeInfo, FirmwareProp},
    resource::{ClkSpec, IrqSpec, MmioRegion, ResourceMap, Virq},
};

/// Domain-allocated device identity (monotonic, per-`DeviceDomain`).
///
/// A newtype: device ids, class publication ids and devnode ids are three
/// DISTINCT u64 spaces — bare `u64` at every boundary is how the class
/// layer once derived an owner from the wrong space (the off-by-one bug).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub(crate) u64);

impl DeviceId {
    /// Raw value (diagnostics, record keys that tag the space explicitly).
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The device's live identity card — the shared, domain-owned side of a
/// registered device (Linux's refcounted `struct device` the class layer
/// points at). Created by the domain at `add_device`; class publications
/// and consumers hold `Arc`s and read facts LIVE instead of carrying
/// value snapshots that drift.
///
/// The authoritative home of "which driver bound this device", "where it
/// sits on the bus topology", and "is it still there". One fact, one
/// home: a device with three class publications has ONE name, ONE path,
/// ONE offline flag.
pub(crate) struct DeviceCore {
    /// Set by the domain when a driver's probe starts (the claim is known
    /// then, not at bind-commit).
    driver_name: kspin::SpinNoPreempt<Option<&'static str>>,
    /// The bus-vocabulary path component, derived once at `add_device`.
    physical_path: alloc::string::String,
    /// Flipped by the removal orchestration (after quiesce, before the
    /// class publications withdraw). Every publication of this device —
    /// and any consumer holding a clone — observes it.
    offline: core::sync::atomic::AtomicBool,
}

impl DeviceCore {
    /// The driver that claimed this device, once probing started.
    pub(crate) fn driver_name(&self) -> Option<&'static str> {
        *self.driver_name.lock()
    }

    /// Record the claiming driver. Domain-internal.
    pub(crate) fn bind_driver(&self, name: &'static str) {
        *self.driver_name.lock() = Some(name);
    }

    /// The physical path (DEVPATH source), as derived from the bus data.
    pub(crate) fn physical_path(&self) -> &str {
        self.physical_path.as_str()
    }

    /// Whether the device is still standing behind its publications.
    pub(crate) fn is_online(&self) -> bool {
        !self.offline.load(core::sync::atomic::Ordering::Acquire)
    }

    /// Mark the device gone. Removal-orchestration internal.
    pub(crate) fn mark_offline(&self) {
        self.offline
            .store(true, core::sync::atomic::Ordering::Release);
    }

    /// A synthetic core for tests and model-external devices: same shape,
    /// no domain behind it. Production paths always publish under a core
    /// the domain created.
    #[cfg(unittest)]
    pub(crate) fn synthetic(
        id: DeviceId,
        driver_name: &'static str,
        physical_path: alloc::string::String,
    ) -> alloc::sync::Arc<Self> {
        let _ = id;
        alloc::sync::Arc::new(Self {
            driver_name: kspin::SpinNoPreempt::new(Some(driver_name)),
            physical_path,
            offline: core::sync::atomic::AtomicBool::new(false),
        })
    }

    pub(crate) fn new(
        _id: DeviceId,
        physical_path: alloc::string::String,
    ) -> alloc::sync::Arc<Self> {
        alloc::sync::Arc::new(Self {
            driver_name: kspin::SpinNoPreempt::new(None),
            physical_path,
            offline: core::sync::atomic::AtomicBool::new(false),
        })
    }
}

/// Handle to one device (`struct device *`).
///
/// THE one device type: probe receives `&mut Device<'_, B>`, remove receives
/// the same plus `&mut Self::Data`. There is no second, remove-only variant
/// to learn — a transcribed driver carries `dev` through both callbacks.
///
/// The surface is split into four views, one per role, so a reader (or a
/// model) never has to hold 25 flat methods in mind:
///
/// | view | answers | Linux analogue |
/// |---|---|---|
/// | [`DeviceInfo`] (`dev.info()`) | which device is this | `dev_name`/`dev->of_node` identity |
/// | [`Resources`] (`dev.res()`) | what may I acquire from it | `platform_get_*`, `devm_*` |
/// | [`Firmware`] (`dev.fw()`) | what did firmware say | `of_property_read_*` |
/// | [`Environment`] (`dev.env()`) | how do I talk to the framework | `-EPROBE_DEFER`, provider publication, extensions |
///
/// Class publication is the fifth verb and stays on the handle:
/// [`Device::publish`].
pub struct Device<'a, B: Bus> {
    /// The enumeration product: identity, named resources, firmware
    /// snapshot, DMA view and topology. One home for every device FACT; the
    /// handle does not copy fields out of it.
    desc: &'a crate::DeviceDescriptor,
    /// The bus's own data vocabulary — typed: probe runs where the bus is
    /// statically known, so drivers read bus facts without any erasure.
    bus_data: &'a B::DeviceData,
    id: DeviceId,
    modalias: Option<&'a str>,
    services: &'a dyn DeviceServices,
    /// The live device identity: what class publications attach to.
    core: alloc::sync::Arc<DeviceCore>,
    _bus: PhantomData<fn(&B)>,
}

impl<'a, B: Bus> Device<'a, B> {
    /// Constructed by the domain when it probes or removes a device.
    pub(crate) fn new(
        desc: &'a crate::DeviceDescriptor,
        bus_data: &'a B::DeviceData,
        id: DeviceId,
        modalias: Option<&'a str>,
        services: &'a dyn DeviceServices,
        core: alloc::sync::Arc<DeviceCore>,
    ) -> Self {
        Self {
            desc,
            bus_data,
            id,
            modalias,
            services,
            core,
            _bus: PhantomData,
        }
    }

    /// Identity: id, path, driver name, modalias, liveness, bus data.
    pub fn info(&self) -> DeviceInfo<'a, B> {
        DeviceInfo {
            id: self.id,
            bus_data: self.bus_data,
            modalias: self.modalias,
            core: self.core.clone(),
            _bus: PhantomData,
        }
    }

    /// Resources: the firmware-stated table and the `devm_*` acquisition
    /// door for MMIO/IRQ/DMA/clocks.
    pub fn res(&self) -> Resources<'a, B> {
        Resources {
            id: self.id,
            map: &self.desc.resources,
            dma: self.desc.dma,
            services: self.services,
            _bus: PhantomData,
        }
    }

    /// Firmware: the node snapshot (`of_property_read_*`).
    pub fn fw(&self) -> Firmware<'a> {
        Firmware {
            node: self.desc.fw.as_ref(),
        }
    }

    /// Framework interaction: deferral, provider publication, extensions.
    pub fn env(&self) -> Environment<'a, B> {
        Environment {
            services: self.services,
            _bus: PhantomData,
        }
    }

    /// Publish a runtime capability into a device class (`device_create` /
    /// `add_disk`): the ONE class-publication door.
    ///
    /// The returned [`Publication`](crate::Publication) is the owner: store it
    /// in `DriverOps::Data` (first field) and dropping it unpublishes — after
    /// the last in-flight consumer leaves. `dev` supplies the shared device
    /// identity (driver name, DEVPATH, liveness); the class supplies the
    /// runtime type and its hooks.
    pub fn publish<T, H>(
        &self,
        class: &'static crate::class::class_api::Class<T, H>,
        runtime: T,
        name: impl Into<alloc::string::String>,
    ) -> kerrno::KResult<crate::Publication<T>>
    where
        T: Send + Sync + 'static,
        H: crate::class::class_api::ClassHooks<T>,
    {
        self.handle().publish(class, runtime, name)
    }

    /// A `'static` handle to this device's identity.
    ///
    /// `publish()` only needs the shared [`DeviceCore`], not the probe-scoped
    /// `Device`, so a class binding may be created *after* probe returns -- which
    /// is what a device that is re-enumerated (re-plugged, or reset and
    /// re-addressed) requires. The identity strings are copied here for the same
    /// reason: nothing in the returned handle borrows this `Device`.
    pub fn handle(&self) -> DeviceHandle {
        DeviceHandle {
            id: self.id,
            core: self.core.clone(),
            path: alloc::string::String::from(self.core.physical_path()),
            modalias: self.modalias.map(alloc::string::String::from),
        }
    }
}

/// A device's identity, detached from the probe call that produced it.
///
/// Class publications attach to [`DeviceCore`], so holding this handle is enough to
/// publish or withdraw a binding at any later point. It names no bus and no device
/// family: it is `Arc<DeviceCore>` plus the two strings that identity projects
/// (`DEVPATH` and `MODALIAS`).
#[derive(Clone)]
pub struct DeviceHandle {
    id: DeviceId,
    core: alloc::sync::Arc<DeviceCore>,
    path: alloc::string::String,
    modalias: Option<alloc::string::String>,
}

#[cfg(unittest)]
impl DeviceHandle {
    /// A handle for tests: the same shape a probe gets, with no domain and no
    /// record behind it. Tests that need a real registry build it themselves.
    pub fn synthetic(driver_name: &'static str, physical_path: &str) -> Self {
        let path = alloc::string::String::from(physical_path);
        Self {
            id: DeviceId(0),
            core: DeviceCore::synthetic(DeviceId(0), driver_name, path.clone()),
            path,
            modalias: None,
        }
    }
}

impl DeviceHandle {
    pub const fn id(&self) -> DeviceId {
        self.id
    }

    /// The bus-vocabulary path component (`DEVPATH` source).
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn modalias(&self) -> Option<&str> {
        self.modalias.as_deref()
    }

    /// Same contract as [`Device::publish`]: the returned [`Publication`] is the
    /// owner, and dropping it withdraws the binding.
    pub fn publish<T, H>(
        &self,
        class: &'static crate::class::class_api::Class<T, H>,
        runtime: T,
        name: impl Into<alloc::string::String>,
    ) -> kerrno::KResult<crate::Publication<T>>
    where
        T: Send + Sync + 'static,
        H: crate::class::class_api::ClassHooks<T>,
    {
        class.publish(self.core.clone(), runtime, name)
    }
}

/// Identity view (`struct device`'s identity half).
pub struct DeviceInfo<'a, B: Bus> {
    id: DeviceId,
    bus_data: &'a B::DeviceData,
    modalias: Option<&'a str>,
    core: alloc::sync::Arc<DeviceCore>,
    _bus: PhantomData<fn(&B)>,
}

impl<B: Bus> DeviceInfo<'_, B> {
    /// The domain-allocated device id.
    pub const fn id(&self) -> DeviceId {
        self.id
    }

    /// The bus-vocabulary path component (DEVPATH source).
    pub fn path(&self) -> &str {
        self.core.physical_path()
    }

    /// The `MODALIAS` derived from the device's own bus data.
    pub fn modalias(&self) -> Option<&str> {
        self.modalias
    }

    /// The bus's device data — typed: probe runs where the bus is statically
    /// known.
    pub const fn bus_data(&self) -> &B::DeviceData {
        self.bus_data
    }
}

/// Resources view: the `platform_get_*` / `devm_*` surface.
///
/// Every acquisition returns an owned guard; store all of them in
/// `DriverOps::Data`, whose field order is the release order.
pub struct Resources<'a, B: Bus> {
    id: DeviceId,
    map: &'a ResourceMap,
    dma: crate::resource::DmaInfo,
    services: &'a dyn DeviceServices,
    _bus: PhantomData<fn(&B)>,
}

impl<B: Bus> Resources<'_, B> {
    /// The whole named resource table.
    pub const fn map(&self) -> &ResourceMap {
        self.map
    }

    /// `platform_get_resource(pdev, IORESOURCE_MEM, n)`.
    pub fn mmio(&self, index: usize) -> Result<MmioRegion, ProbeError> {
        self.map
            .mmio(index)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))
    }

    /// `platform_get_resource_byname(pdev, IORESOURCE_MEM, name)`.
    pub fn mmio_by_name(&self, name: &str) -> Result<MmioRegion, ProbeError> {
        self.map
            .mmio_by_name(name)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))
    }

    /// `platform_get_irq(pdev, n)`.
    pub fn irq(&self, index: usize) -> Result<IrqSpec, ProbeError> {
        self.map
            .irq(index)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))
    }

    /// `platform_get_irq_byname(pdev, name)`.
    pub fn irq_by_name(&self, name: &str) -> Result<IrqSpec, ProbeError> {
        self.map
            .irq_by_name(name)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))
    }

    /// Resolve an irq resource to its OS-visible virtual irq without
    /// registering a handler.
    ///
    /// Drivers whose handlers filter by irq number MUST go through here — a
    /// firmware route's `hwirq` is controller-relative, not the virq. The
    /// [`Virq`] newtype makes the two impossible to mix up: only
    /// [`Virq::raw`] crosses back into a plain number.
    pub fn resolve_irq(&self, index: usize) -> Result<Virq, ProbeError> {
        let spec = self.irq(index)?;
        self.services
            .map_irq(self.id, spec)
            .map_err(ProbeError::from)
    }

    /// `devm_ioremap_resource(dev, res)` on the n-th MMIO region.
    pub fn iomap(&self, index: usize) -> Result<IoMap, ProbeError> {
        let region = self.mmio(index)?;
        self.iomap_region(region, "mmio")
    }

    /// `devm_platform_ioremap_resource(pdev, index)` variant that resolves
    /// `reg-names` first.
    pub fn iomap_named(&self, name: &str) -> Result<IoMap, ProbeError> {
        let region = self.mmio_by_name(name)?;
        self.iomap_region(region, "mmio")
    }

    /// Map an explicit region (used by PCI BARs resolved by the bus).
    pub fn iomap_region(
        &self,
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
        &self,
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

    /// [`Self::request_irq`] on the n-th interrupt resource.
    pub fn request_irq_at<F>(&self, index: usize, handler: F) -> Result<DevIrq, ProbeError>
    where
        F: Fn() -> IrqAction + Send + Sync + 'static,
    {
        let spec = self.irq(index)?;
        self.request_irq(spec, "irq", handler)
    }

    /// [`Self::request_irq`] on an `interrupt-names` entry.
    pub fn request_irq_by_name<F>(&self, name: &str, handler: F) -> Result<DevIrq, ProbeError>
    where
        F: Fn() -> IrqAction + Send + Sync + 'static,
    {
        let spec = self.irq_by_name(name)?;
        self.request_irq(spec, "irq", handler)
    }

    /// `dmam_alloc_coherent(dev, size, dma_handle, gfp)` — the device's own
    /// DMA view travels with the request; the returned
    /// `CoherentMem::bus_addr` is the DEVICE-visible address.
    pub fn alloc_coherent(&self, len: usize, align: usize) -> Result<CoherentMem, ProbeError> {
        self.services
            .alloc_coherent(self.id, self.dma, len, align)
            .map_err(ProbeError::Fatal)
    }

    /// n-th clock input, in `clocks = <...>` declaration order (for
    /// nodes whose clocks carry no `clock-names`).
    pub fn clk_at(&self, index: usize) -> Result<ClkHandle, ProbeError> {
        let spec = self
            .map
            .clk_at(index)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        self.clk_spec(spec)
    }

    /// `phy_get(dev, name)`: resolves a `phy-names` entry and acquires
    /// the phy; returns [`ProbeError::Deferred`] while the provider has
    /// not published.
    pub fn phy(&self, name: &str) -> Result<PhyHandle, ProbeError> {
        let spec = self
            .map
            .phy_by_name(name)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        self.phy_spec(spec)
    }

    /// [`Self::phy`] on the n-th `phys` entry (declaration order).
    pub fn phy_at(&self, index: usize) -> Result<PhyHandle, ProbeError> {
        let spec = self
            .map
            .phy_at(index)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        self.phy_spec(spec)
    }

    /// [`Self::phy`] on an explicit spec.
    pub fn phy_spec(&self, spec: crate::resource::PhySpec) -> Result<PhyHandle, ProbeError> {
        self.services
            .phy_get(self.id, spec)
            .map_err(ProbeError::from)
    }

    /// `reset_control_get(dev, id)`: resolves a `reset-names` entry and
    /// acquires the line; returns [`ProbeError::Deferred`] while the reset
    /// controller has not published yet.
    pub fn reset(&self, name: &str) -> Result<ResetHandle, ProbeError> {
        let spec = self
            .map
            .reset_by_name(name)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        self.reset_spec(spec)
    }

    /// [`Self::reset`] on the n-th `resets` entry (declaration order).
    pub fn reset_at(&self, index: usize) -> Result<ResetHandle, ProbeError> {
        let spec = self
            .map
            .reset_at(index)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        self.reset_spec(spec)
    }

    /// [`Self::reset`] on an explicit spec.
    pub fn reset_spec(&self, spec: crate::resource::ResetSpec) -> Result<ResetHandle, ProbeError> {
        self.services
            .reset_get(self.id, spec)
            .map_err(ProbeError::from)
    }

    /// `clk_get(dev, id)`: resolves a `clock-names` entry and acquires the
    /// clock; returns [`ProbeError::Deferred`] while the provider is missing.
    pub fn clk(&self, name: &str) -> Result<ClkHandle, ProbeError> {
        let spec = self
            .map
            .clk_by_name(name)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        self.clk_spec(spec)
    }

    /// [`Self::clk`] on an explicit spec.
    pub fn clk_spec(&self, spec: ClkSpec) -> Result<ClkHandle, ProbeError> {
        self.services
            .clk_get(self.id, spec)
            .map_err(ProbeError::from)
    }
}

/// Firmware view: `of_property_read_*` over the node snapshot.
pub struct Firmware<'a> {
    node: Option<&'a FirmwareNodeInfo>,
}

impl Firmware<'_> {
    /// The node snapshot itself, when the device came from firmware.
    pub const fn node(&self) -> Option<&FirmwareNodeInfo> {
        self.node
    }

    /// `of_property_read_*` on the snapshot (decoded on read).
    pub fn prop(&self, name: &str) -> Option<FirmwareProp> {
        self.node?.prop(name)
    }

    /// `of_property_read_u32` on the snapshot.
    pub fn u32(&self, name: &str) -> Option<u32> {
        self.prop(name).and_then(|prop| prop.as_u32())
    }

    /// `of_property_read_string` on the snapshot.
    pub fn string(&self, name: &str) -> Option<&'static str> {
        match self.prop(name)? {
            FirmwareProp::Str(value) => Some(value),
            _ => None,
        }
    }

    /// `of_property_read_bool` on the snapshot: presence is the value.
    pub fn flag(&self, name: &str) -> bool {
        self.prop(name).is_some()
    }
}

/// Framework-interaction view: deferral, provider publication, extensions.
pub struct Environment<'a, B: Bus> {
    services: &'a dyn DeviceServices,
    _bus: PhantomData<fn(&B)>,
}

impl<B: Bus> Environment<'_, B> {
    /// Build a `-EPROBE_DEFER` return value for an out-of-band dependency.
    pub fn defer(&self, dep: DepSource) -> ProbeError {
        ProbeError::Deferred(dep)
    }

    /// Look up a family-specific service extension on the installed host
    /// (e.g. the virtio transport service). The core names no family: the
    /// family registers its extension value at host install time, and the
    /// driver asks by type.
    pub fn extension<E: ExtensionService>(&self) -> Option<&'static E> {
        self.services.extensions().get::<E>()
    }

    /// Publish a provider this device owns (`of_clk_add_hw_provider`,
    /// `gpiochip_add_data`, `regulator_register`): the resource OTHER drivers
    /// acquire by firmware handle, and the thing their probe defers on until
    /// it appears.
    ///
    /// `C` is the provider family's [`ProviderClass`](crate::ProviderClass)
    /// descriptor; `key` is the class-scoped handle consumers name in their
    /// firmware reference; `lookup` is the class's lookup object. This is the
    /// single provider-registration door.
    ///
    /// Call it as the LAST fallible step of a provider driver's probe: a
    /// registration that wakes parked consumers cannot be rolled back by
    /// returning `Err` afterwards.
    pub fn provide<C: crate::ProviderClass>(
        &self,
        key: C::Key,
        lookup: &'static C::Lookup,
    ) -> Result<(), ProbeError> {
        self.services
            .register_provider(crate::ProviderRegistration::new::<C>(key, lookup))
            .map_err(ProbeError::Fatal)
    }
}
