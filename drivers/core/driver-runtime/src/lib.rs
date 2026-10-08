// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host implementation of [`driver_core::DeviceServices`] plus the clock
//! provider registry.
//!
//! This is the only place where the new device model touches the existing
//! `device-res` provider stack: the x-kernel provider is wrapped once, behind
//! the per-device service surface that drivers actually see. When the
//! resource layer is later folded into `driver-runtime` proper (refactor
//! section 4.4), driver-facing code does not change.
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

#[cfg(feature = "virtio-hal")]
pub mod hal;
#[cfg(feature = "virtio-hal")]
pub mod virtio_service;

use alloc::sync::Arc;

pub use clk::ClkRegistry;
use device_res::{
    DmaAllocation, DmaOp, DmaSpec, IrqEvent as ResIrqEvent, IrqHandler as ResIrqHandler, IrqOp,
    IrqResource, MmioMapping, MmioOp, ResError,
};
use device_res_xkernel::XKernelResourceProvider;
use driver_core::{
    ClkHandle, CoherentMem, DevIrq, DeviceServices, IoMap, IrqAction, IrqHandlerFn, RuntimeError,
    ServiceError,
};

static GLOBAL_RUNTIME: kspin::SpinNoPreempt<Option<Arc<XKernelRuntime>>> =
    kspin::SpinNoPreempt::new(None);

/// Publish a clock controller through the composition-root runtime.
///
/// Driver-visible entry used by clock-provider drivers inside `probe`;
/// equivalent to `runtime.register_clk_provider` on the installed global
/// runtime. Wakes every probe parked on this provider's phandle.
pub fn register_clk_provider(provider: &'static dyn kclk::ClkLookup) {
    let runtime = GLOBAL_RUNTIME
        .lock()
        .clone()
        .expect("driver-runtime: XKernelRuntime::install_global must run first");
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
            // kirq has no threaded-handler vocabulary yet (the review
            // defers threaded IRQ); WakeThread degrades to Handled.
            IrqAction::WakeThread => ResIrqEvent::HANDLED,
        }
    }
}

/// The x-kernel service runtime: one device-res provider plus the clk
/// registry, presented as [`DeviceServices`].
///
/// Probe-time services carry the requesting device id so per-device
/// accounting, DMA attributes, and interrupt routing stay possible as the
/// resource layer is folded in.
pub struct XKernelRuntime {
    provider: Arc<XKernelResourceProvider>,
    clks: ClkRegistry,
}

impl XKernelRuntime {
    /// The resource provider handle (for service extensions that need
    /// direct provider access without going through the global runtime).
    pub(crate) fn provider_arc(&self) -> Arc<XKernelResourceProvider> {
        self.provider.clone()
    }

    /// Build the runtime with a wake hook that re-probes the given domain
    /// when a clock provider registers.
    pub fn new(wake: impl Fn(driver_core::DepSource) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            provider: Arc::new(XKernelResourceProvider::new()),
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
        self.provider
            .map_mmio(
                device_res::MmioRegion {
                    base: region.base,
                    size: region.size,
                },
                "driver-core",
            )
            .map_err(map_res_error)
    }
}

impl DeviceServices for XKernelRuntime {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn iomap(
        &self,
        _device: u64,
        region: driver_core::MmioRegion,
        _name: &'static str,
    ) -> Result<IoMap, RuntimeError> {
        let mapping = self.iomap_resource(region)?;
        let release = {
            let provider = self.provider.clone();
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
        // Preserve the firmware route: with controller (+ hwirq) present the
        // provider maps through the controller's irq domain; a bare number
        // becomes PlainVirq and assumes the bus already resolved it.
        let mut resource = IrqResource::new(spec.number, spec.trigger);
        resource.controller = spec.controller;
        resource.hwirq = spec.hwirq;
        let adapted: Arc<dyn ResIrqHandler> = Arc::new(AdaptedIrqHandler { handler });
        let token = self
            .provider
            .request_irq(resource, adapted)
            .map_err(map_res_error)?;
        let release = {
            let provider = self.provider.clone();
            move || provider.release_irq(resource, token)
        };
        let _ = name;
        Ok(DevIrq::new(spec.number, Some(Arc::new(release))))
    }

    fn alloc_coherent(
        &self,
        _device: u64,
        len: usize,
        align: usize,
    ) -> Result<CoherentMem, RuntimeError> {
        let spec = DmaSpec::new(len.max(1), align.max(1));
        let allocation: DmaAllocation =
            self.provider.alloc_coherent(spec).map_err(map_res_error)?;
        let release = {
            let provider = self.provider.clone();
            move || provider.free_coherent(allocation)
        };
        Ok(CoherentMem::new(
            allocation.cpu_addr,
            allocation.bus_addr,
            spec.len,
            Some(Arc::new(release)),
        ))
    }

    fn clk_get(&self, _device: u64, spec: driver_core::ClkSpec) -> Result<ClkHandle, ServiceError> {
        self.clks.get(spec)
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
    ) -> Result<(driver_core::DeviceKind, virtio::PciTransport, Option<usize>), RuntimeError> {
        crate::probe_pci_transport_with(&self.provider, descriptor)
    }
}

/// Provider-parameterized transport probe: the PciTransportService calls
/// this with the provider recovered from ITS services instance, so a
/// non-global domain binds its own provider.
pub fn probe_pci_transport_with(
    provider: &Arc<XKernelResourceProvider>,
    descriptor: &driver_core::desc::DeviceDescriptor,
) -> Result<(driver_core::DeviceKind, virtio::PciTransport, Option<usize>), RuntimeError> {
    let _self = ();
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

        // IRQ: the transport-bound vector (x86 MSI-X) first; otherwise the
        // descriptor's legacy route (resolved once at discovery) mapped to
        // a virtual irq by the platform provider.

        // Discovery already configured the function (BARs assigned, memory
        // decoded). Only per-device config access remains for the virtio
        // capability walk.
        let mut pci_root =
            PciHost::new(pci::support::pci_cam_kind()).map_err(|_| RuntimeError::Io)?;
        let root = pci_root.root_mut();

        // The vendored constructor maps the device's MMIO regions through
        // the HAL; the probe scope gathers those mapping leases so they can
        // ride with the transport (released when it drops) instead of
        // leaking at the trait boundary. On failure the leases drop here and
        // the mappings die with the failed probe.
        // Scope guard: the slot closes and collected leases drop even if
        // the vendored constructor panics mid-walk (the old begin/take pair
        // left the slot stuck open on panic).
        let scope = hal::begin_mmio_probe();
        let probed =
            virtio::probe_pci_transport_only::<hal::Hal>(root, bdf, &dev_info, provider.as_ref());
        let (kind, mut transport, transport_irq) = probed.ok_or(RuntimeError::BadState)?;
        let leases = scope.take_leases();
        transport.retain_mappings(leases);

        let irq = transport_irq.or_else(|| {
            descriptor
                .resources
                .irq(0)
                .and_then(|spec| virtio::resolve_legacy_irq(&spec, provider.as_ref()))
        });

        Ok((kind, transport, irq))
    }
}
