// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared helpers for the virtio driver modules (error mapping, class
//! parent location conversion) — one copy instead of ten.

use alloc::string::String;

use driver_core::{ProbeError, RuntimeError, desc::DeviceLocation};

pub(crate) fn map_driver_error(err: driver_base::DriverError) -> ProbeError {
    let error = match err {
        driver_base::DriverError::InvalidInput | driver_base::DriverError::ReadOnly => {
            RuntimeError::Invalid
        }
        driver_base::DriverError::NoMemory => RuntimeError::NoMemory,
        driver_base::DriverError::ResourceBusy | driver_base::DriverError::AlreadyExists => {
            RuntimeError::Busy
        }
        driver_base::DriverError::BadState => RuntimeError::BadState,
        driver_base::DriverError::Io | driver_base::DriverError::WouldBlock => RuntimeError::Io,
        driver_base::DriverError::Unsupported => RuntimeError::Unsupported,
    };
    ProbeError::Fatal(error)
}

/// Format the DEVPATH physical path from a location (the same string the
/// devnode registry stores).
pub(crate) fn physical_path(location: DeviceLocation) -> String {
    // The DEVPATH format lives in devnode exactly once. `devnode` re-exports
    // the location type from driver-core, so this passes the SAME type
    // straight through — no adapter or conversion layer.
    devnode::physical_path(&location)
}

/// Probe this device's virtio PCI transport through the runtime's
/// `PciTransportService` extension: kind + concrete transport + irq, with
/// the PCI/provider boilerplate owned by the service. Compile-time typed —
/// the wrong transport type cannot get past the compiler.
pub(crate) fn pci_transport<B: driver_core::Bus>(
    dev: &mut driver_core::DeviceCtx<'_, B>,
) -> Result<(driver_core::DeviceKind, virtio::PciTransport, Option<usize>), driver_core::ProbeError>
{
    let service = dev
        .extension::<driver_runtime::virtio_service::PciTransportService>()
        .ok_or(driver_core::ProbeError::Fatal(
            driver_core::RuntimeError::Unsupported,
        ))?;
    service
        .probe(dev.descriptor())
        .map_err(driver_core::ProbeError::Fatal)
}
