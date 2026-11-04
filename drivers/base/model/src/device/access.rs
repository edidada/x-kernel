// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Typed device access and its callback-scoped information and service views.

use alloc::sync::Arc;
use core::marker::PhantomData;

use super::DeviceRef;
use crate::{
    ClkHandle, CoherentMem, DevIrq, DeviceServices, IoMap, IrqEvent,
    bus::Bus,
    error::{ProbeError, RuntimeError},
    firmware::{FirmwareNodeInfo, FirmwareProp},
    resource::{ClkSpec, IrqSpec, MmioRegion, ResourceMap, Virq},
};

/// Callback-scoped, typed access to one registered device.
///
/// Borrows identity and host services without owning device state or the
/// acquired resource guards. Framework-wide services are passed separately
/// through [`Environment`](crate::Environment).
///
/// The scoped callback view: probe receives `&mut DeviceAccess<'_, B>`, remove receives
/// the same plus `&mut Self::Data`. There is no second, remove-only variant
/// to learn — a transcribed driver carries `dev` through both callbacks.
/// A [`DeviceRef`] retains only identity and enumeration facts beyond them.
///
/// The surface is split into three views, one per role, so a reader (or a
/// model) never has to hold 25 flat methods in mind:
///
/// | view | answers | Linux analogue |
/// |---|---|---|
/// | [`DeviceInfo`] (`dev.info()`) | which device is this | `dev_name`/`dev->of_node` identity |
/// | [`Resources`] (`dev.res()`) | what may I acquire from it | `platform_get_*`, `devm_*` |
/// | [`Firmware`] (`dev.fw()`) | what did firmware say | `of_property_read_*` |
///
/// Subsystems obtain immutable publication provenance through
/// [`DeviceAccess::publication_source`]; this grants no hardware access.
pub struct DeviceAccess<'a, B: Bus> {
    /// The bus's own data vocabulary — typed: probe runs where the bus is
    /// statically known, so drivers read bus facts without any erasure.
    bus_data: &'a B::DeviceData,
    modalias: Option<&'a str>,
    services: &'a dyn DeviceServices,
    /// The live device identity: what class publications attach to.
    reference: &'a DeviceRef,
    driver: &'a Arc<crate::driver::DriverIdentity>,
    _bus: PhantomData<fn(&B)>,
}

impl<'a, B: Bus> DeviceAccess<'a, B> {
    /// Constructed by the domain when it probes or removes a device.
    /// Returns `None` when the device's bus payload has the wrong concrete type.
    pub(crate) fn new(
        reference: &'a DeviceRef,
        driver: &'a Arc<crate::driver::DriverIdentity>,
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
            driver,
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

    /// Retain this callback's driver identity and device metadata as provenance.
    ///
    /// Later unbind/rebind cannot change the reported driver. This source keeps
    /// no ops, binding Data or acquired hardware resources alive. Its online
    /// bit describes device registration, not permission to access hardware.
    pub fn publication_source(&self) -> Arc<dyn super::PublicationSource> {
        Arc::new(super::source::BindingSource {
            device: self.reference.clone(),
            driver: self.driver.clone(),
        })
    }
}

/// Borrowed identity view (`struct device`'s identity half).
/// Use [`DeviceAccess::reference`] to retain identity beyond the callback view.
pub struct DeviceInfo<'a, B: Bus> {
    device: &'a DeviceAccess<'a, B>,
}

impl<B: Bus> DeviceInfo<'_, B> {
    /// The bus-defined location component, not a complete DEVPATH or unique key.
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
    /// The callback receives the OS-visible virtual IRQ, never a hardware IRQ.
    pub fn request_irq<F>(
        &self,
        spec: IrqSpec,
        name: &'static str,
        handler: F,
    ) -> Result<DevIrq, ProbeError>
    where
        F: Fn(usize) -> IrqEvent + Send + Sync + 'static,
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
        F: Fn(usize) -> IrqEvent + Send + Sync + 'static,
    {
        let spec = self.irq(index)?;
        self.request_irq(spec, "irq", handler)
    }

    /// [`Self::request_irq`] on an `interrupt-names` entry.
    pub fn request_irq_by_name<F>(&self, name: &str, handler: F) -> Result<DevIrq, ProbeError>
    where
        F: Fn(usize) -> IrqEvent + Send + Sync + 'static,
    {
        let spec = self.irq_by_name(name)?;
        self.request_irq(spec, "irq", handler)
    }

    /// Capture the device's DMA capability beyond this callback borrow.
    ///
    /// The host retains the stable descriptor view and original provider, not
    /// this callback. The driver must still stop hardware before guard cleanup.
    ///
    /// # Errors
    /// Unconfigured DMA fails before host dispatch; host errors remain fatal.
    pub fn dma(&self) -> Result<Arc<dyn crate::DeviceDma>, ProbeError> {
        self.device
            .descriptor()
            .dma
            .ok_or(ProbeError::Fatal(RuntimeError::Unsupported))?;
        self.services
            .dma(crate::DeviceContext::new(self.device))
            .map_err(ProbeError::Fatal)
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
