// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host implementation of [`driver_core::DeviceServices`] plus the clock
//! provider registry.
//!
//! This crate owns the resource provider (formerly the separate `device-res`
//! stack, folded in here by the device-res refactor): it is bound once, behind
//! the per-device service surface that drivers actually see. Driver-facing
//! code does not change whether the provider lives here or in a dedicated
//! crate — the seam is [`DeviceServices`].
//!
//! # Clock integration
//!
//! `kclk` (ported from the `clk/tail` branch) registers clock controllers
//! through [`ClkRegistry::register_provider`] — the analogue of
//! `of_clk_add_hw_provider`. Consumers call `dev.clk("name")`; while the
//! provider is missing the probe defers and is woken exactly when the
//! provider registers.
//!
//! ```ignore
//! let runtime = XKernelRuntime::install(global_domain());
//! // kclk provider driver, after building its clock tree:
//! runtime.register_clk_provider(phandle, clk_ops);
//! ```

#![no_std]

extern crate alloc;

mod clk;

pub mod provider;
#[cfg(feature = "virtio-hal")]
pub mod virtio_hal;
#[cfg(feature = "virtio-hal")]
pub mod virtio_transport;

use alloc::sync::Arc;

pub use clk::ClkRegistry;
use driver_core::{
    ClkHandle, ClockConsumerServices, CoherentMem, DevIrq, DeviceResourceServices, DeviceServices,
    IoMap, IrqAction, IrqHandlerFn, RuntimeError, ServiceError,
    provider::{DmaAllocation, DmaOp, DmaSpec, IrqOp, IrqResource, MmioMapping, MmioOp, ResError},
};
use kirq::{IrqEvent as ResIrqEvent, IrqHandler as ResIrqHandler};
use provider::XKernelResourceProvider;

static GLOBAL_RUNTIME: kspin::SpinNoPreempt<Option<Arc<XKernelRuntime>>> =
    kspin::SpinNoPreempt::new(None);

/// Publish a clock controller through the composition-root runtime.
///
/// Driver-visible entry used by clock-provider drivers inside `probe`;
/// equivalent to `runtime.register_clk_provider` on the installed global
/// runtime. Wakes every probe parked on this provider's phandle.
pub fn register_clk_provider(provider: &'static dyn kclk::ClkLookup) {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: boot-order discipline — the \\
                  composition root installs the global runtime before any driver runs"
    )]
    let runtime = GLOBAL_RUNTIME
        .lock()
        .clone()
        .expect("driver-host: XKernelRuntime::install_global must run first");
    runtime.register_clk_provider(provider);
}

static RESOURCE_PROVIDER: XKernelResourceProvider = XKernelResourceProvider::new();

/// The process-wide x-kernel resource provider as a `&'static` — for
/// transport HALs and class services that still address the provider
/// directly during the migration.
pub fn resource_provider() -> &'static XKernelResourceProvider {
    &RESOURCE_PROVIDER
}

fn map_res_error(err: ResError) -> RuntimeError {
    match err {
        ResError::InvalidResource => RuntimeError::Invalid,
        ResError::MappingFailed => RuntimeError::Io,
        ResError::NoMemory => RuntimeError::NoMemory,
        ResError::Busy => RuntimeError::Busy,
        ResError::Unsupported => RuntimeError::Unsupported,
    }
}

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

/// The x-kernel service runtime: one host resource provider plus the clk
/// registry, presented as [`DeviceServices`].
///
/// Probe-time services carry the requesting device id so per-device
/// accounting, DMA attributes, and interrupt routing stay possible as the
/// resource layer is folded in.
pub struct XKernelRuntime {
    /// The ONE process-wide provider — the same instance the transport
    /// HAL addresses via [`resource_provider`]. The vendored virtio `Hal`
    /// trait exposes associated functions (no `&self` receiver), so a HAL
    /// cannot carry a per-runtime provider instance; a second,
    /// runtime-private `Arc::new(provider)` would silently fork the
    /// provider world (per-device accounting, future IOMMU domains) from
    /// the HAL's. Both sides therefore share this single static.
    provider: &'static XKernelResourceProvider,
    clks: ClkRegistry,
}

impl XKernelRuntime {
    /// The resource provider handle (for service extensions that need
    /// direct provider access without going through the global runtime).
    pub(crate) fn static_provider(&self) -> &'static XKernelResourceProvider {
        self.provider
    }

    /// Build the runtime with a wake hook that re-probes the given domain
    /// when a clock provider registers.
    pub fn new(wake: impl Fn(driver_core::DepSource) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            provider: resource_provider(),
            clks: ClkRegistry::new(wake),
        })
    }

    /// Convenience for the composition root: wires the global device domain
    /// as the deferred-probe wake target and installs the runtime there.
    pub fn install_global() -> Arc<Self> {
        let runtime = Self::new(|dep| {
            driver_core::global_domain().wake(dep);
        });
        driver_core::global_domain().set_services(runtime.clone());
        *GLOBAL_RUNTIME.lock() = Some(runtime.clone());
        runtime
    }

    /// Register a clock controller (`of_clk_add_hw_provider` analogue) and
    /// wake every probe that was waiting for it.
    ///
    /// The lookup must live for the kernel's lifetime, matching the kclk
    /// registry's boot-static model — a controller driver typically leaks
    /// or statics its table adapter.
    pub fn register_clk_provider(&self, provider: &'static dyn kclk::ClkLookup) {
        self.clks.register_provider(provider);
    }

    fn iomap_resource(&self, region: driver_core::MmioRegion) -> Result<MmioMapping, RuntimeError> {
        // driver-core's MmioRegion is the single address vocabulary — one
        // type, no field-by-field re-construction.
        self.provider
            .map_mmio(region, "driver-core")
            .map_err(map_res_error)
    }
}

impl DeviceResourceServices for XKernelRuntime {
    fn iomap(
        &self,
        _device: u64,
        region: driver_core::MmioRegion,
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
        _device: u64,
        spec: driver_core::IrqSpec,
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
            driver_core::IrqSpec::ResolvedVirq { number, trigger } => {
                IrqResource::plain(number, trigger)
            }
            driver_core::IrqSpec::FirmwareRoute {
                controller,
                domain,
                hwirq,
                trigger,
            } => {
                let route = provider::irq::to_runtime_route(controller, domain, hwirq, trigger);
                let mapped = self.provider.map_irq(route).map_err(map_res_error)?;
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
            .map_err(map_res_error)?;
        let number = resource.number();
        let release = {
            let provider = self.provider;
            move || provider.release_irq(resource, token)
        };
        let _ = name;
        Ok(DevIrq::new(number, Some(Arc::new(release))))
    }

    fn alloc_coherent(
        &self,
        _device: u64,
        dma: driver_core::DmaInfo,
        len: usize,
        align: usize,
    ) -> Result<CoherentMem, RuntimeError> {
        let spec = DmaSpec::new(len.max(1), align.max(1));
        let allocation: DmaAllocation =
            self.provider.alloc_coherent(spec).map_err(map_res_error)?;
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

    fn map_irq(&self, _device: u64, spec: driver_core::IrqSpec) -> Result<usize, RuntimeError> {
        match spec {
            driver_core::IrqSpec::ResolvedVirq { number, .. } => Ok(number),
            driver_core::IrqSpec::FirmwareRoute {
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
                    .map(|irq| irq.number())
                    .map_err(map_res_error)
            }
        }
    }
}

impl ClockConsumerServices for XKernelRuntime {
    fn clk_get(&self, _device: u64, spec: driver_core::ClkSpec) -> Result<ClkHandle, ServiceError> {
        self.clks.get(spec)
    }
}

impl DeviceServices for XKernelRuntime {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl XKernelRuntime {
    /// Probe a virtio PCI transport for a device location (the
    /// `PciTransportService` extension backend). One place owns the PCI +
    /// provider boilerplate every virtio driver would otherwise duplicate.
    #[cfg(feature = "virtio-hal")]
    pub fn probe_pci_transport(
        &self,
        descriptor: &driver_core::desc::DeviceDescriptor,
    ) -> Result<
        (
            driver_core::DeviceKind,
            virtio_devices::PciTransport,
            Option<usize>,
        ),
        RuntimeError,
    > {
        crate::probe_pci_transport_with(self.provider, descriptor)
    }
}

/// Provider-parameterized transport probe: the PciTransportService calls
/// this with the provider recovered from ITS services instance, so a
/// non-global domain binds its own provider.
#[cfg(feature = "virtio-hal")]
pub fn probe_pci_transport_with(
    provider: &'static XKernelResourceProvider,
    descriptor: &driver_core::desc::DeviceDescriptor,
) -> Result<
    (
        driver_core::DeviceKind,
        virtio_devices::PciTransport,
        Option<usize>,
    ),
    RuntimeError,
> {
    {
        use driver_core::desc::{DeviceIdentity, DeviceLocation as Loc};
        use pci::{DeviceFunction, DeviceFunctionInfo, PciHost};

        let Loc::Pci {
            bus: bus_nr,
            device: dev_nr,
            function,
            ..
        } = descriptor.location
        else {
            return Err(RuntimeError::Invalid);
        };
        let bdf = DeviceFunction {
            bus: bus_nr,
            device: dev_nr,
            function,
        };

        // Identity comes from the descriptor (filled once at discovery) —
        // no bus re-enumeration here. prog_if/revision/header/multifunction
        // are not consumed by virtio transport setup.
        let DeviceIdentity::Pci(pci_identity) = &descriptor.identity else {
            return Err(RuntimeError::Invalid);
        };
        let dev_info = DeviceFunctionInfo {
            vendor_id: pci_identity.vendor_id,
            device_id: pci_identity.device_id,
            class: pci_identity.class,
            subclass: pci_identity.subclass,
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
            descriptor
                .resources
                .irq(0)
                .and_then(|spec| crate::provider::irq::resolve_irq_spec(provider, &spec))
        });

        Ok((kind, transport, irq))
    }
}
