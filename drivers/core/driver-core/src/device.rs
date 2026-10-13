// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The probe-side surface: `DeviceCtx` — the one object a driver holds.

use alloc::sync::Arc;
use core::marker::PhantomData;

use crate::{
    bus::Bus,
    desc::{DeviceDescriptor, FirmwareProp},
    error::{DepSource, ProbeError, RuntimeError},
    handle::{ClkHandle, CoherentMem, DevIrq, IoMap, IrqAction},
    resource::{ClkSpec, IrqSpec, MmioRegion},
    services::{DeviceServices, ExtensionService},
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

    /// Resolve an irq resource to its OS-visible virtual irq (the number
    /// the host IRQ core dispatches) without registering a handler.
    /// Drivers whose handlers filter by irq number MUST go through here —
    /// a firmware route's `hwirq` is controller-relative, not the virq.
    pub fn resolve_irq(&self, index: usize) -> Result<usize, ProbeError> {
        let spec = self.irq(index)?;
        self.services
            .map_irq(self.id, spec)
            .map_err(ProbeError::from)
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

    /// `dmam_alloc_coherent(dev, size, dma_handle, gfp)` — the device's own
    /// DMA view (mask/offset/coherency from the descriptor) travels with
    /// the request; the returned `CoherentMem::bus_addr` is the
    /// DEVICE-visible address (CPU-physical translated through
    /// `dma-ranges`), not the host allocation's raw paddr.
    pub fn alloc_coherent(&mut self, len: usize, align: usize) -> Result<CoherentMem, ProbeError> {
        self.services
            .alloc_coherent(self.id, self.descriptor.dma, len, align)
            .map_err(ProbeError::Fatal)
    }

    /// Look up one firmware node property by name (`of_property_read_*`):
    /// the enumeration-time snapshot of the node, eagerly typed. Returns
    /// `None` for devices with no firmware description (static devices)
    /// or when the property is absent — the `vendor,foo-capability` quirk
    /// path, answerable at probe without re-walking firmware.
    pub fn fw_prop(&self, name: &str) -> Option<&FirmwareProp> {
        self.descriptor.firmware.as_ref()?.prop(name)
    }

    /// `of_property_read_u32` on the snapshot.
    pub fn fw_u32(&self, name: &str) -> Option<u32> {
        self.fw_prop(name).and_then(FirmwareProp::as_u32)
    }

    /// `of_property_read_string` on the snapshot.
    pub fn fw_str(&self, name: &str) -> Option<&'static str> {
        match self.fw_prop(name)? {
            FirmwareProp::Str(value) => Some(*value),
            _ => None,
        }
    }

    /// `of_property_read_bool` on the snapshot: presence is the value.
    pub fn fw_flag(&self, name: &str) -> bool {
        self.fw_prop(name).is_some()
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
