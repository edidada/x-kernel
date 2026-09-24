// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The virtio transport service extension.
//!
//! Device families (drv-virtio) obtain this through
//! `DeviceCtx::extension::<PciTransportService>()`. The concrete transport
//! type appears in THIS trait's signature — a driver naming the wrong type
//! fails at compile time, not at probe — and driver-core stays free of any
//! device-family knowledge.

use driver_core::{DeviceKind, ExtensionService, desc::DeviceLocation};

use crate::XKernelRuntime;

/// Probes a virtio PCI transport: configures the function, builds the
/// transport, resolves the legacy/MSI irq. One implementation owns the PCI
/// and provider boilerplate that every virtio driver would otherwise
/// duplicate.
pub struct PciTransportService {
    runtime: alloc::sync::Arc<XKernelRuntime>,
}

impl PciTransportService {
    /// Probe the transport for a device location.
    pub fn probe(
        &self,
        location: DeviceLocation,
    ) -> Result<(DeviceKind, virtio::PciTransport, Option<usize>), driver_core::RuntimeError> {
        self.runtime.probe_pci_transport(location)
    }
}

impl ExtensionService for PciTransportService {
    fn from_services(services: &dyn driver_core::DeviceServices) -> Option<Self> {
        // Ensure the runtime is the installed global one (its provider and
        // clock registry are the live instances), then capture its Arc.
        let _ = services.as_any().downcast_ref::<XKernelRuntime>()?;
        let runtime = crate::global_runtime_arc()?;
        Some(Self { runtime })
    }
}
