// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The process-level [`Host`]: kernel services behind the device model, plus
//! the provider registries those services own.
//!
//! This crate owns the resource provider (formerly the separate `device-res`
//! stack, folded in here): it is bound once, behind the per-device service
//! surface that drivers actually see. Driver-facing code does not change
//! whether the provider lives here or in a dedicated crate — the seam is
//! [`DeviceServices`].
//!
//! [`Host`] is the ONE home of process-global wiring: the installed services,
//! the resource provider, and the provider registries. Nothing else in the
//! tree may keep a parallel global (the old `GLOBAL_RUNTIME` /
//! `RESOURCE_PROVIDER` free functions are gone; [`Host::global`] is the only
//! accessor).
//!
//! # Provider registries
//!
//! `kclk` (ported from the `clk/tail` branch) is the first
//! [`ProviderClass`](driver_base::ProviderClass) instance: `driver_host::ClkProvider`.
//! A clock controller's probe ends with
//!
//! ```ignore
//! dev.env().provide::<driver_host::ClkProvider>(phandle, lookup)?;
//! ```
//!
//! consumers call `dev.res().clk("name")`, and while the provider is missing the
//! probe defers and is woken exactly when that key is published — the
//! `of_clk_add_hw_provider` / `-EPROBE_DEFER` handshake, one door for every
//! provider family.

#![no_std]

extern crate alloc;

mod clk;

pub mod provider;
#[cfg(feature = "virtio-hal")]
pub mod virtio_hal;
#[cfg(feature = "virtio-hal")]
pub mod virtio_transport;

use alloc::sync::Arc;

pub use clk::{ClkProvider, ClkRegistry};
use driver_base::{
    ClkHandle, ClockConsumerServices, CoherentMem, DevIrq, DeviceResourceServices, DeviceServices,
    ExtensionRegistry, ExtensionServices, IoMap, IrqAction, IrqHandlerFn, RuntimeError,
    ServiceError, Virq,
    devres::host_api::{DmaAllocation, DmaOp, DmaSpec, IrqOp, IrqResource, MmioMapping, MmioOp},
};
use kirq::{IrqEvent as ResIrqEvent, IrqHandler as ResIrqHandler};
use provider::XKernelResourceProvider;

static GLOBAL_HOST: kspin::SpinNoPreempt<Option<Arc<Host>>> = kspin::SpinNoPreempt::new(None);

static RESOURCE_PROVIDER: XKernelResourceProvider = XKernelResourceProvider::new();

struct AdaptedIrqHandler {
    handler: IrqHandlerFn,
}

impl ResIrqHandler for AdaptedIrqHandler {
    fn handle(&self, _irq: usize) -> ResIrqEvent {
        match (self.handler)() {
            IrqAction::Handled => ResIrqEvent::HANDLED,
            IrqAction::NotHandled => ResIrqEvent::NOT_HANDLED,
        }
    }
}

/// The process-level host: one resource provider, the provider registries,
/// and the accessors the composition root and the transport HAL use.
///
/// Probe-time services carry the requesting device id so per-device
/// accounting, DMA attributes, and interrupt routing stay possible as the
/// resource layer is folded in.
pub struct Host {
    /// The ONE process-wide provider — the same instance the transport
    /// HAL addresses via [`Host::provider`]. The vendored virtio `Hal`
    /// trait exposes associated functions (no `&self` receiver), so a HAL
    /// cannot carry a per-runtime provider instance; a second,
    /// runtime-private `Arc::new(provider)` would silently fork the
    /// provider world (per-device accounting, future IOMMU domains) from
    /// the HAL's. Both sides therefore share this single static.
    provider: &'static XKernelResourceProvider,
    clks: ClkRegistry,
    extensions: ExtensionRegistry,
}

impl Host {
    /// The resource provider this host is bound to. The vendored virtio `Hal`
    /// trait exposes associated functions, so it addresses the provider
    /// through this accessor rather than carrying one; there is exactly one
    /// provider world per host.
    pub const fn provider(&self) -> &'static XKernelResourceProvider {
        self.provider
    }

    /// The device domain this host serves (the model's boot-static domain).
    pub fn domain(&self) -> &'static driver_base::DeviceDomain {
        driver_base::global_domain()
    }

    /// The installed host. Boot-order assertion: the composition root
    /// installs it before any driver runs.
    pub fn global() -> Arc<Self> {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: boot-order discipline — the \
                      composition root installs the global host before any driver runs"
        )]
        let host = GLOBAL_HOST
            .lock()
            .clone()
            .expect("driver-host: Host::install_global must run first");
        host
    }

    /// Build the runtime with a wake hook that re-probes the given domain
    /// when a clock provider registers.
    pub fn new(wake: impl Fn(driver_base::DepSource) + Send + Sync + 'static) -> Arc<Self> {
        let extensions = ExtensionRegistry::new();
        #[cfg(feature = "virtio-hal")]
        extensions.register(alloc::boxed::Box::leak(alloc::boxed::Box::new(
            virtio_transport::PciTransportService::new(&RESOURCE_PROVIDER),
        )));
        Arc::new(Self {
            provider: &RESOURCE_PROVIDER,
            clks: ClkRegistry::new(wake),
            extensions,
        })
    }

    /// Install this host as the process-level one, wiring the device domain
    /// as the deferred-probe wake target. Called once, by the composition
    /// root.
    pub fn install_global() -> Arc<Self> {
        let host = Self::new(|dep| {
            driver_base::global_domain().wake(dep);
        });
        driver_base::global_domain().set_services(host.clone());
        *GLOBAL_HOST.lock() = Some(host.clone());
        host
    }

    fn iomap_resource(&self, region: driver_base::MmioRegion) -> Result<MmioMapping, RuntimeError> {
        // driver-core's MmioRegion is the single address vocabulary — one
        // type, no field-by-field re-construction.
        self.provider
            .map_mmio(region, "driver-core")
            .map_err(RuntimeError::from)
    }
}

impl DeviceResourceServices for Host {
    fn iomap(
        &self,
        _device: driver_base::DeviceId,
        region: driver_base::MmioRegion,
        _name: &'static str,
    ) -> Result<IoMap, RuntimeError> {
        let mapping = self.iomap_resource(region)?;
        let release = {
            let provider = self.provider;
            move || provider.unmap_mmio(mapping)
        };
        // SAFETY: the provider mapped the region; the handle pairs the
        // provider's release hook.
        Ok(unsafe { IoMap::new(mapping.vaddr, region, Some(Arc::new(release))) })
    }

    fn request_irq(
        &self,
        _device: driver_base::DeviceId,
        spec: driver_base::IrqSpec,
        name: &'static str,
        handler: IrqHandlerFn,
    ) -> Result<DevIrq, RuntimeError> {
        // Preserve the route shape end to end — and resolve firmware
        // routes BEFORE registering: the provider maps through the
        // controller's domain (reusing an existing mapping when one
        // exists), so the registered resource and the returned DevIrq
        // carry the REAL virq, never a placeholder 0. A
        // `DomainMapped { number: 0 }` here would hand the driver an
        // irq guard whose number is not the irq the host dispatches.
        let resource = match spec {
            driver_base::IrqSpec::ResolvedVirq { virq, trigger } => {
                IrqResource::plain(virq, trigger)
            }
            driver_base::IrqSpec::FirmwareRoute {
                controller,
                domain,
                hwirq,
                trigger,
            } => {
                let route = provider::irq::to_runtime_route(controller, domain, hwirq, trigger);
                let mapped = self.provider.map_irq(route).map_err(RuntimeError::from)?;
                match mapped {
                    IrqResource::DomainMapped { .. } => mapped,
                    // The provider's map returned a plain resource for a
                    // route — treat as its resolved number with the route
                    // origin lost; acceptable only for identity domains.
                    plain @ IrqResource::PlainVirq { .. } => plain,
                }
            }
        };
        let adapted: Arc<dyn ResIrqHandler> = Arc::new(AdaptedIrqHandler { handler });
        let token = self
            .provider
            .request_irq(resource, adapted)
            .map_err(RuntimeError::from)?;
        let virq = resource.virq();
        let release = {
            let provider = self.provider;
            move || provider.release_irq(resource, token)
        };
        let _ = name;
        Ok(DevIrq::new(virq, Some(Arc::new(release))))
    }

    fn alloc_coherent(
        &self,
        _device: driver_base::DeviceId,
        dma: driver_base::DmaInfo,
        len: usize,
        align: usize,
    ) -> Result<CoherentMem, RuntimeError> {
        let spec = DmaSpec::new(len.max(1), align.max(1));
        let allocation: DmaAllocation = self
            .provider
            .alloc_coherent(spec)
            .map_err(RuntimeError::from)?;
        // The provider returns CPU-physical identity; the DEVICE-visible
        // address goes through this device's dma-ranges translation and
        // must fit under its dma_mask — an allocation outside the device's
        // view is a failure (Linux dma_direct_alloc retry/fail), never a
        // silently truncated bus address.
        let Some(bus_addr) = dma.translate(allocation.bus_addr) else {
            self.provider.free_coherent(allocation);
            return Err(RuntimeError::NoMemory);
        };
        // dmam_alloc_coherent zeroes (Linux parity); on a device the
        // firmware declared non-coherent, the writes must then be pushed
        // out of CPU caches before the device can see the buffer.
        // SAFETY: the provider returned `spec.len` bytes exclusively
        // owned by this allocation.
        unsafe {
            core::ptr::write_bytes(allocation.cpu_addr as *mut u8, 0, spec.len);
        }
        if !dma.coherent {
            // SAFETY: the range is the provider-returned exclusive buffer
            // of `spec.len` bytes at `cpu_addr`.
            unsafe {
                self.provider.flush_cache(
                    core::ptr::NonNull::new_unchecked(allocation.cpu_addr as *mut u8),
                    spec.len,
                );
            }
        }
        let release = {
            let provider = self.provider;
            move || provider.free_coherent(allocation)
        };
        Ok(CoherentMem::new(
            allocation.cpu_addr,
            bus_addr,
            spec.len,
            Some(Arc::new(release)),
        ))
    }

    fn map_irq(
        &self,
        _device: driver_base::DeviceId,
        spec: driver_base::IrqSpec,
    ) -> Result<Virq, RuntimeError> {
        match spec {
            driver_base::IrqSpec::ResolvedVirq { virq, .. } => Ok(virq),
            driver_base::IrqSpec::FirmwareRoute {
                controller,
                domain,
                hwirq,
                trigger,
            } => {
                // Map through the controller's irq domain (reusing an
                // existing mapping when one exists) — the same
                // resolution request_irq performs, available without
                // registering a handler.
                let route = provider::irq::to_runtime_route(controller, domain, hwirq, trigger);
                self.provider
                    .map_irq(route)
                    .map(|irq| irq.virq())
                    .map_err(RuntimeError::from)
            }
        }
    }
}

impl driver_base::ProviderServices for Host {
    fn register_provider(
        &self,
        registration: driver_base::ProviderRegistration,
    ) -> Result<(), RuntimeError> {
        match registration.kind() {
            driver_base::DepKind::Clk => {
                let key = registration.key();
                let lookup = registration
                    .lookup::<ClkProvider>()
                    .ok_or(RuntimeError::Invalid)?;
                // The publication key and the lookup's own id must agree:
                // a mismatch would wake consumers parked on a key nobody
                // serves and leave the real ones parked forever.
                if u64::from(lookup.provider_id()) != key {
                    return Err(RuntimeError::Invalid);
                }
                self.clks.register_provider(lookup)
            }
            _ => Err(RuntimeError::Unsupported),
        }
    }
}

impl ClockConsumerServices for Host {
    fn clk_get(
        &self,
        _device: driver_base::DeviceId,
        spec: driver_base::ClkSpec,
    ) -> Result<ClkHandle, ServiceError> {
        self.clks.get(spec)
    }
}

impl ExtensionServices for Host {
    fn extensions(&self) -> &ExtensionRegistry {
        &self.extensions
    }
}

impl DeviceServices for Host {}

impl Host {
    /// Probe a virtio PCI transport for a device location (the
    /// `PciTransportService` extension backend). One place owns the PCI +
    /// provider boilerplate every virtio driver would otherwise duplicate.
    #[cfg(feature = "virtio-hal")]
    pub fn probe_pci_transport(
        &self,
        data: &dyn driver_base::BusDeviceData,
        resources: &driver_base::ResourceMap,
    ) -> Result<
        (
            virtio_devices::VirtioDeviceKind,
            virtio_devices::PciTransport,
            Option<usize>,
        ),
        RuntimeError,
    > {
        crate::probe_pci_transport_with(self.provider, data, resources)
    }
}

/// Provider-parameterized transport probe: the PciTransportService calls
/// this with the provider recovered from ITS services instance, so a
/// non-global domain binds its own provider.
#[cfg(feature = "virtio-hal")]
pub fn probe_pci_transport_with(
    provider: &'static XKernelResourceProvider,
    data: &dyn driver_base::BusDeviceData,
    resources: &driver_base::ResourceMap,
) -> Result<
    (
        virtio_devices::VirtioDeviceKind,
        virtio_devices::PciTransport,
        Option<usize>,
    ),
    RuntimeError,
> {
    {
        use pci::{DeviceFunction, DeviceFunctionInfo, PciBusData, PciHost};

        // The PCI bus owns this device's vocabulary — recover it through
        // the vtable (bus_id was checked at match time).
        let Some(pci_data) = data.as_any().downcast_ref::<PciBusData>() else {
            return Err(RuntimeError::Invalid);
        };
        let bdf = DeviceFunction {
            bus: pci_data.bus,
            device: pci_data.device,
            function: pci_data.function,
        };

        // Identity comes from the descriptor (filled once at discovery) —
        // no bus re-enumeration here. prog_if/revision/header/multifunction
        // are not consumed by virtio transport setup.
        let dev_info = DeviceFunctionInfo {
            vendor_id: pci_data.vendor_id,
            device_id: pci_data.device_id,
            class: pci_data.class,
            subclass: pci_data.subclass,
            prog_if: 0,
            revision: 0,
            header_type: pci::HeaderType::Standard,
        };

        // Discovery already configured the function (BARs assigned, memory
        // decoded). Only per-device config access remains for the virtio
        // capability walk.
        let mut pci_root =
            PciHost::new(pci::device_setup::pci_cam_kind()).map_err(|_| RuntimeError::Io)?;
        let root = pci_root.root_mut();

        // The vendored constructor maps the device's MMIO regions through
        // the HAL; the probe scope gathers those mapping leases so they can
        // ride with the transport (released when it drops) instead of
        // leaking at the trait boundary. On failure the leases drop here and
        // the mappings die with the failed probe.
        // Scope guard: the slot closes and collected leases drop even if
        // the vendored constructor panics mid-walk (the old begin/take pair
        // left the slot stuck open on panic).
        let scope = virtio_hal::begin_mmio_probe();
        let probed = virtio_devices::probe_pci_transport_only::<virtio_hal::Hal>(
            root, bdf, &dev_info, provider,
        );
        let (kind, mut transport, transport_irq) = probed.ok_or(RuntimeError::BadState)?;
        let leases = scope.take_leases();
        transport.retain_mappings(leases);

        // Legacy INTx fallback: the descriptor's firmware route resolves
        // through the controller's irq domain here, in the host bridge —
        // the family library no longer carries provider vocabulary.
        let irq = transport_irq.or_else(|| {
            resources
                .irq(0)
                .and_then(|spec| crate::provider::irq::resolve_irq_spec(provider, &spec))
                .map(Virq::raw)
        });

        Ok((kind, transport, irq))
    }
}
