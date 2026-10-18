// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared helpers for the virtio driver modules (error mapping) — one copy
//! instead of ten.

use driver_base::{ProbeError, RuntimeError};
use kerrno::KError;
use virtio_devices::VirtioDeviceKind;

/// One activated virtio device: the family's owned state (publication,
/// guards, transport), closed in the Linux remove order:
///
/// ```text
/// quiesce hardware -> withdraw class publication -> release IRQ
///     -> drop transport -> unmap MMIO registers
/// ```
///
/// Both the PCI path (erased in `Data`) and the mmio path (owned by
/// `MmioData`) use this one trait; there is no second teardown vocabulary.
pub trait Activation: Send + Sync {
    /// Tear the device down in the order above.
    fn close(self: alloc::boxed::Box<Self>);
}

pub(crate) fn map_driver_error(err: KError) -> ProbeError {
    let error = match err {
        KError::InvalidInput | KError::ReadOnlyFilesystem => RuntimeError::Invalid,
        KError::NoMemory => RuntimeError::NoMemory,
        KError::ResourceBusy | KError::AlreadyExists => RuntimeError::Busy,
        KError::BadState => RuntimeError::BadState,
        KError::Io | KError::WouldBlock => RuntimeError::Io,
        KError::Unsupported => RuntimeError::Unsupported,
        // The class layer's vocabulary is the former DriverError set; any
        // other kerrno code is foreign here — treat it as an I/O failure.
        _ => RuntimeError::Io,
    };
    ProbeError::Fatal(error)
}

/// Probe this device's virtio PCI transport through the runtime's
/// `PciTransportService` extension: kind + concrete transport + irq, with
/// the PCI/provider boilerplate owned by the service. Compile-time typed —
/// the wrong transport type cannot get past the compiler.
pub(crate) fn pci_transport<B: driver_base::Bus>(
    dev: &mut driver_base::Device<'_, B>,
) -> Result<
    (
        VirtioDeviceKind,
        virtio_devices::PciTransport,
        Option<usize>,
    ),
    driver_base::ProbeError,
> {
    let service = dev
        .env()
        .extension::<driver_host::virtio_transport::PciTransportService>()
        .ok_or(driver_base::ProbeError::Fatal(
            driver_base::RuntimeError::Unsupported,
        ))?;
    service
        .probe(dev.info().bus_data(), dev.res().map())
        .map_err(driver_base::ProbeError::Fatal)
}
