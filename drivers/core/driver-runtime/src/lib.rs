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
//! # Clock integration (Phase 0 acceptance path)
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

use alloc::sync::Arc;

pub use clk::ClkRegistry;
use device_res::{
    DmaAllocation, DmaOp, DmaSpec, IrqEvent as ResIrqEvent, IrqHandler as ResIrqHandler, IrqOp,
    IrqResource, IrqTrigger as ResIrqTrigger, MmioMapping, MmioOp, ResError,
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

/// The provider as a `&'static dyn TimeOp` (virtio-rng and other
/// time-consuming device constructors).
pub fn time_ops() -> &'static dyn device_res::TimeOp {
    &RESOURCE_PROVIDER
}

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

fn map_trigger(trigger: driver_core::IrqTrigger) -> ResIrqTrigger {
    use driver_core::IrqTrigger as Ours;
    match trigger {
        Ours::EdgeRising => ResIrqTrigger::EdgeRising,
        Ours::EdgeFalling => ResIrqTrigger::EdgeFalling,
        Ours::LevelHigh => ResIrqTrigger::LevelHigh,
        Ours::LevelLow => ResIrqTrigger::LevelLow,
        Ours::Unknown => ResIrqTrigger::Unknown(0),
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
            IrqAction::WakeThread => ResIrqEvent::WAKE_THREAD,
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
        Ok(IoMap::new(mapping.vaddr, region, Some(Arc::new(release))))
    }

    fn request_irq(
        &self,
        _device: u64,
        spec: driver_core::IrqSpec,
        name: &'static str,
        handler: IrqHandlerFn,
    ) -> Result<DevIrq, RuntimeError> {
        let resource = IrqResource::new(spec.number, map_trigger(spec.trigger));
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
