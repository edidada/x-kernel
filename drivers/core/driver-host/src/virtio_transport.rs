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

use driver_core::{DeviceKind, ExtensionService, desc::DeviceDescriptor};

use crate::XKernelRuntime;

/// Probes a virtio PCI transport: configures the function, builds the
/// transport, resolves the legacy/MSI irq. One implementation owns the PCI
/// and provider boilerplate that every virtio driver would otherwise
/// duplicate.
pub struct PciTransportService {
    /// The PROVIDER handle recovered from the services instance this
    /// extension was requested from — never the global fallback. A domain
    /// with its own runtime binds its own provider (review II-6).
    provider: &'static crate::XKernelResourceProvider,
}

impl PciTransportService {
    /// Probe the transport for a device descriptor.
    pub fn probe(
        &self,
        descriptor: &DeviceDescriptor,
    ) -> Result<(DeviceKind, virtio_devices::PciTransport, Option<usize>), driver_core::RuntimeError>
    {
        crate::probe_pci_transport_with(self.provider, descriptor)
    }
}

impl ExtensionService for PciTransportService {
    fn from_services(services: &dyn driver_core::DeviceServices) -> Option<Self> {
        // Recover the extension from THIS services instance, not a global
        // fallback: the type check plus `global_runtime_arc()` bound
        // domain A's devices to the global runtime even when the domain
        // carries its own (review II-6). The probe path needs only the
        // provider handle.
        let runtime: &XKernelRuntime = services.as_any().downcast_ref()?;
        Some(Self {
            provider: runtime.static_provider(),
        })
    }
}
