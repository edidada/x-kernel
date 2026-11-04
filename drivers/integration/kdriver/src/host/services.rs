// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Compose native host capabilities for a portable device domain.
//!
//! Construction does not install global services or enumerate devices. IRQ
//! core integration is supplied through the existing OS capability contract.

use alloc::sync::Arc;

use driver_model::{
    ClkHandle, ClkSpec, ClockConsumerServices, CoherentMem, DevIrq, DeviceContext,
    DeviceManagementLock, DeviceResourceServices, DeviceServices, ExtensionRegistry,
    ExtensionServices, IoMap, IrqHandler, IrqSpec, MmioRegion, ProviderRegistration,
    ProviderServices, RuntimeError, ServiceError, Virq,
};
use driver_os_api::{DeviceDma, IrqOp, IrqResource, IrqRouteDesc};

/// Compose MMIO, per-device DMA, management locks and a supplied IRQ capability.
///
/// Install the result on a domain before registering devices. Each call owns an
/// independent extension table; no per-device resource facts are cached here.
/// Clock consumption and provider publication return `Unsupported` until those
/// families have host integration. Construction follows the kernel Arc OOM policy.
///
/// The IRQ provider must implement real domain mapping, and its release operation
/// must synchronously detach and drain the exact returned registration. Guards
/// retain that provider until release and must be dropped in its permitted
/// teardown context, after the driver stops hardware. This composition does not
/// validate IRQ-core implementation or change its enable/mask policy.
pub fn new_device_services(irq: Arc<dyn IrqOp>) -> Arc<dyn DeviceServices> {
    Arc::new(HostDeviceServices {
        irq,
        extensions: ExtensionRegistry::new(),
    })
}

/// Compose native MMIO, DMA, management locks and ordinary shared IRQ services.
///
/// This only constructs capabilities: it neither installs them globally nor
/// enumerates or probes devices. IRQ domains and the scheduler must be ready
/// before use. Requests require sleepable task context; first registration uses
/// kirq's auto-enable policy. IRQ guards must be dropped exactly once in sleepable
/// task context after hardware stops, without locks needed by handlers. Release
/// synchronously drains the exact registration and panics on failure rather
/// than permitting unsafe reclamation. See [`driver_os_api_xkernel::NativeIrq`].
pub fn new_native_device_services() -> Arc<dyn DeviceServices> {
    new_device_services(Arc::new(driver_os_api_xkernel::NativeIrq))
}

struct HostDeviceServices {
    irq: Arc<dyn IrqOp>,
    extensions: ExtensionRegistry,
}

impl HostDeviceServices {
    fn resolve_irq(&self, spec: IrqSpec) -> Result<IrqResource, RuntimeError> {
        match spec {
            IrqSpec::ResolvedVirq { virq, trigger } => Ok(IrqResource::plain(virq, trigger)),
            IrqSpec::FirmwareRoute {
                controller,
                domain,
                hwirq,
                trigger,
            } => self
                .irq
                .map_irq(IrqRouteDesc {
                    controller,
                    domain,
                    hwirq,
                    trigger,
                })
                .map_err(RuntimeError::from),
        }
    }
}

impl DeviceResourceServices for HostDeviceServices {
    fn dma(&self, device: DeviceContext<'_>) -> Result<Arc<dyn DeviceDma>, RuntimeError> {
        crate::host_dma::new_device_dma(device)
    }

    fn iomap(
        &self,
        device: DeviceContext<'_>,
        region: MmioRegion,
        name: &'static str,
    ) -> Result<IoMap, RuntimeError> {
        crate::host_mmio::iomap(device, region, name)
    }

    fn request_irq(
        &self,
        _: DeviceContext<'_>,
        spec: IrqSpec,
        _: &'static str,
        handler: Arc<dyn IrqHandler>,
    ) -> Result<DevIrq, RuntimeError> {
        let resource = self.resolve_irq(spec)?;
        DevIrq::request_with_shared_provider(self.irq.clone(), resource, handler)
            .map_err(RuntimeError::from)
    }

    fn alloc_coherent(
        &self,
        device: DeviceContext<'_>,
        len: usize,
        align: usize,
    ) -> Result<CoherentMem, RuntimeError> {
        crate::host_dma::alloc_coherent(device, len, align)
    }

    fn map_irq(&self, _: DeviceContext<'_>, spec: IrqSpec) -> Result<Virq, RuntimeError> {
        self.resolve_irq(spec).map(|resource| resource.virq())
    }
}

impl DeviceServices for HostDeviceServices {
    fn create_management_lock(&self) -> Result<Arc<dyn DeviceManagementLock>, RuntimeError> {
        super::management::new_device_management_lock().map_err(RuntimeError::from)
    }
}

impl ExtensionServices for HostDeviceServices {
    fn extensions(&self) -> &ExtensionRegistry {
        &self.extensions
    }
}

impl ClockConsumerServices for HostDeviceServices {
    fn clk_get(&self, _: DeviceContext<'_>, _: ClkSpec) -> Result<ClkHandle, ServiceError> {
        Err(RuntimeError::Unsupported.into())
    }
}

impl ProviderServices for HostDeviceServices {
    fn register_provider(&self, _: ProviderRegistration) -> Result<(), RuntimeError> {
        Err(RuntimeError::Unsupported)
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/host_services.rs");
