// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device identity references and scoped, typed probe/remove views.

use alloc::sync::Arc;
use core::marker::PhantomData;

use crate::{
    ClkHandle, CoherentMem, DevIrq, DeviceServices, IoMap, IrqAction,
    bus::Bus,
    domain::DeviceId,
    error::{DepSource, ProbeError, RuntimeError},
    extension::ExtensionService,
    firmware::{FirmwareNodeInfo, FirmwareProp},
    resource::{ClkSpec, IrqSpec, MmioRegion, ResourceMap, Virq},
};

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
    pub(crate) id: DeviceId,
    pub(crate) desc: crate::DeviceDescriptor,
    /// Set by the domain when a driver's probe starts (the claim is known
    /// then, not at bind-commit).
    driver_name: kspin::SpinNoPreempt<Option<&'static str>>,
    /// The bus-vocabulary path component, derived once at `add_device`.
    physical_path: alloc::string::String,
    /// Flipped by the removal orchestration (before quiesce and before the
    /// class publications withdraw). Every publication of this device —
    /// and any consumer holding a clone — observes it.
    offline: core::sync::atomic::AtomicBool,
}

impl driver_class::PublicationSource for DeviceCore {
    fn driver_name(&self) -> Option<&'static str> {
        self.driver_name()
    }

    fn physical_path(&self) -> &str {
        self.physical_path()
    }

    fn is_online(&self) -> bool {
        self.is_online()
    }
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

    pub(crate) fn new(id: DeviceId, desc: crate::DeviceDescriptor) -> alloc::sync::Arc<Self> {
        let physical_path = desc.bus_data.path_component();
        alloc::sync::Arc::new(Self {
            id,
            desc,
            driver_name: kspin::SpinNoPreempt::new(None),
            physical_path,
            offline: core::sync::atomic::AtomicBool::new(false),
        })
    }
}

/// An owning reference to a registered device's identity and immutable facts.
///
/// Cloning this reference does not retain the binding or its resources. After
/// removal, metadata remains readable, but it grants no hardware access.
/// Dropping a reference never unregisters the device. Final metadata destruction
/// runs in the last owner's context; bus data must not require sleepable Drop.
#[derive(Clone)]
pub struct DeviceRef {
    pub(crate) core: Arc<DeviceCore>,
}

impl DeviceRef {
    /// Immutable enumeration facts, retained even after device removal.
    pub fn descriptor(&self) -> &crate::DeviceDescriptor {
        &self.core.desc
    }

    /// Physical location derived from the registered bus data.
    pub fn path(&self) -> &str {
        self.core.physical_path()
    }

    /// The last driver whose probe started, not proof of a current binding.
    pub fn driver_name(&self) -> Option<&'static str> {
        self.core.driver_name()
    }

    /// Whether removal has not yet closed this device's publication source.
    /// This snapshot is neither a binding check nor an in-flight operation permit.
    pub fn is_online(&self) -> bool {
        self.core.is_online()
    }

    /// Whether both references designate the same registered object.
    pub fn same_device(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.core, &other.core)
    }
}

impl core::fmt::Debug for DeviceRef {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("DeviceRef")
            .field("registration_key", &self.core.id)
            .finish_non_exhaustive()
    }
}

/// Handle to one device (`struct device *`).
///
/// The scoped callback view: probe receives `&mut Device<'_, B>`, remove receives
/// the same plus `&mut Self::Data`. There is no second, remove-only variant
/// to learn — a transcribed driver carries `dev` through both callbacks.
/// A [`DeviceRef`] retains only identity and enumeration facts beyond them.
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
    /// The bus's own data vocabulary — typed: probe runs where the bus is
    /// statically known, so drivers read bus facts without any erasure.
    bus_data: &'a B::DeviceData,
    modalias: Option<&'a str>,
    services: &'a dyn DeviceServices,
    /// The live device identity: what class publications attach to.
    reference: &'a DeviceRef,
    _bus: PhantomData<fn(&B)>,
}

impl<'a, B: Bus> Device<'a, B> {
    /// Constructed by the domain when it probes or removes a device.
    /// Returns `None` when the device's bus payload has the wrong concrete type.
    pub(crate) fn new(
        reference: &'a DeviceRef,
        modalias: Option<&'a str>,
        services: &'a dyn DeviceServices,
    ) -> Option<Self> {
        let bus_data = reference
            .descriptor()
            .bus_data
            .as_any()
            .downcast_ref::<B::DeviceData>()?;
        Some(Self {
            bus_data,
            modalias,
            services,
            reference,
            _bus: PhantomData,
        })
    }

    /// Retain identity and static facts, without retaining driver resources.
    pub fn reference(&self) -> DeviceRef {
        self.reference.clone()
    }

    /// Borrow identity information without retaining the device independently.
    pub fn info(&self) -> DeviceInfo<'_, B> {
        DeviceInfo { device: self }
    }

    /// Resources: the firmware-stated table and the `devm_*` acquisition
    /// door for MMIO/IRQ/DMA/clocks.
    pub fn res(&self) -> Resources<'_, B> {
        Resources {
            device: self.reference,
            services: self.services,
            _bus: PhantomData,
        }
    }

    /// Firmware: the node snapshot (`of_property_read_*`).
    pub fn fw(&self) -> Firmware<'a> {
        Firmware {
            node: self.reference.descriptor().fw.as_ref(),
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
    /// Store the returned Registration in DriverOps::Data. Dropping it closes
    /// admission immediately; class teardown waits for existing permits.
    /// This does not delay destruction of Data's other fields. The runtime must
    /// own resources needed by operations, or remove must join those operations
    /// before releasing resources. Field order alone is not sufficient.
    pub fn publish<T, H>(
        &self,
        class: &'static driver_class::Class<T, H>,
        runtime: T,
    ) -> kerrno::KResult<driver_class::Registration<T>>
    where
        T: Send + Sync + 'static,
        H: driver_class::ClassHooks<T>,
    {
        class.publish(self.reference.core.clone(), runtime)
    }
}

/// Borrowed identity view (`struct device`'s identity half).
/// Use [`Device::reference`] to retain identity beyond the callback view.
pub struct DeviceInfo<'a, B: Bus> {
    device: &'a Device<'a, B>,
}

impl<B: Bus> DeviceInfo<'_, B> {
    /// The bus-vocabulary path component (DEVPATH source).
    pub fn path(&self) -> &str {
        self.device.reference.path()
    }

    /// The `MODALIAS` derived from the device's own bus data.
    pub fn modalias(&self) -> Option<&str> {
        self.device.modalias
    }

    /// The bus's device data — typed: probe runs where the bus is statically
    /// known.
    pub const fn bus_data(&self) -> &B::DeviceData {
        self.device.bus_data
    }
}

/// Resources view: the `platform_get_*` / `devm_*` surface.
///
/// Every acquisition returns an owned guard; store all of them in
/// `DriverOps::Data`, whose field order is the release order.
pub struct Resources<'a, B: Bus> {
    device: &'a DeviceRef,
    services: &'a dyn DeviceServices,
    _bus: PhantomData<fn(&B)>,
}

impl<B: Bus> Resources<'_, B> {
    /// The whole named resource table.
    pub fn map(&self) -> &ResourceMap {
        &self.device.descriptor().resources
    }

    /// `platform_get_resource(pdev, IORESOURCE_MEM, n)`.
    pub fn mmio(&self, index: usize) -> Result<MmioRegion, ProbeError> {
        self.map()
            .mmio(index)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))
    }

    /// `platform_get_resource_byname(pdev, IORESOURCE_MEM, name)`.
    pub fn mmio_by_name(&self, name: &str) -> Result<MmioRegion, ProbeError> {
        self.map()
            .mmio_by_name(name)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))
    }

    /// `platform_get_irq(pdev, n)`.
    pub fn irq(&self, index: usize) -> Result<IrqSpec, ProbeError> {
        self.map()
            .irq(index)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))
    }

    /// `platform_get_irq_byname(pdev, name)`.
    pub fn irq_by_name(&self, name: &str) -> Result<IrqSpec, ProbeError> {
        self.map()
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
            .map_irq(crate::DeviceContext::new(self.device), spec)
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
            .iomap(crate::DeviceContext::new(self.device), region, name)
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
            .request_irq(
                crate::DeviceContext::new(self.device),
                spec,
                name,
                Arc::new(handler),
            )
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
    ///
    /// # Errors
    /// Returns `ProbeError::Fatal(RuntimeError::Unsupported)` without invoking
    /// the host when DMA is unconfigured. Otherwise propagates host errors.
    pub fn alloc_coherent(&self, len: usize, align: usize) -> Result<CoherentMem, ProbeError> {
        self.device
            .descriptor()
            .dma
            .ok_or(ProbeError::Fatal(RuntimeError::Unsupported))?;
        self.services
            .alloc_coherent(crate::DeviceContext::new(self.device), len, align)
            .map_err(ProbeError::Fatal)
    }

    /// `clk_get(dev, id)`: resolves a `clock-names` entry and acquires the
    /// clock; returns [`ProbeError::Deferred`] while the provider is missing.
    pub fn clk(&self, name: &str) -> Result<ClkHandle, ProbeError> {
        let spec = self
            .map()
            .clk_by_name(name)
            .ok_or(ProbeError::Fatal(RuntimeError::NotFound))?;
        self.clk_spec(spec)
    }

    /// [`Self::clk`] on an explicit spec.
    pub fn clk_spec(&self, spec: ClkSpec) -> Result<ClkHandle, ProbeError> {
        self.services
            .clk_get(crate::DeviceContext::new(self.device), spec)
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
