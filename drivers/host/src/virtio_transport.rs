// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The virtio transport service extension.
//!
//! Device families (drv-virtio) obtain this through
//! `Device::extension::<PciTransportService>()`. The concrete transport
//! type appears in THIS trait's signature — a driver naming the wrong type
//! fails at compile time, not at probe — and driver-core stays free of any
//! device-family knowledge.

use driver_base::{BusDeviceData, ExtensionService};

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

impl ExtensionService for PciTransportService {}

impl PciTransportService {
    /// Construct the extension value the host registers at install time.
    pub(crate) const fn new(provider: &'static crate::XKernelResourceProvider) -> Self {
        Self { provider }
    }

    /// Probe the transport for a device's bus data.
    pub fn probe(
        &self,
        data: &dyn BusDeviceData,
        resources: &driver_base::ResourceMap,
    ) -> Result<
        (
            virtio_devices::VirtioDeviceKind,
            virtio_devices::PciTransport,
            Option<usize>,
        ),
        driver_base::RuntimeError,
    > {
        crate::probe_pci_transport_with(self.provider, data, resources)
    }
}
