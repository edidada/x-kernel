// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared helpers for the virtio driver modules (error mapping, class
//! parent location conversion) — one copy instead of ten.

use alloc::string::String;

use driver_core::{ProbeError, RuntimeError, desc::DeviceLocation};

pub(crate) fn map_driver_error(err: device_traits::DriverError) -> ProbeError {
    let error = match err {
        device_traits::DriverError::InvalidInput | device_traits::DriverError::ReadOnly => {
            RuntimeError::Invalid
        }
        device_traits::DriverError::NoMemory => RuntimeError::NoMemory,
        device_traits::DriverError::ResourceBusy | device_traits::DriverError::AlreadyExists => {
            RuntimeError::Busy
        }
        device_traits::DriverError::BadState => RuntimeError::BadState,
        device_traits::DriverError::Io | device_traits::DriverError::WouldBlock => RuntimeError::Io,
        device_traits::DriverError::Unsupported => RuntimeError::Unsupported,
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
) -> Result<
    (
        driver_core::DeviceKind,
        virtio_devices::PciTransport,
        Option<usize>,
    ),
    driver_core::ProbeError,
> {
    let service = dev
        .extension::<driver_host::virtio_transport::PciTransportService>()
        .ok_or(driver_core::ProbeError::Fatal(
            driver_core::RuntimeError::Unsupported,
        ))?;
    service
        .probe(dev.descriptor())
        .map_err(driver_core::ProbeError::Fatal)
}
