// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-rng driver on the driver-core model (PCI transport).
//!
//! The smallest consumer template: const PCI id table, one class
//! publication (`char`), no interrupt. The entropy subsystem consumes it
//! through the model-agnostic `kclass::char_devices()` view.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;

use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError,
    pci::{PciBus, PciMatch},
};
use kclass::{ClassParent, publish_char};
use pci::{DeviceFunction, PciBus as PciRootAccess};
use virtio::{VirtIoRngDev, probe_pci_device};

/// `struct pci_device_id virtio_rng_id_table[]`: transitional 0x1005 and
/// modern 0x1045.
static IDS: &[PciMatch] = &[
    PciMatch::device(0x1af4, 0x1005),
    PciMatch::device(0x1af4, 0x1045),
];

/// Driver-private data: the published char device itself is owned by the
/// char class registry; there is nothing else to hold.
pub struct RngData;

/// The driver ops.
pub struct VirtioRngDriver;

impl DriverOps<PciBus> for VirtioRngDriver {
    type Data = RngData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PciBus>) -> Result<RngData, ProbeError> {
        use driver_core::desc::DeviceLocation;

        let DeviceLocation::Pci {
            bus: bus_nr,
            device: dev_nr,
            function,
            ..
        } = dev.descriptor().location
        else {
            return Err(ProbeError::Fatal(RuntimeError::Invalid));
        };

        let mut pci_root = PciRootAccess::new(pci::support::pci_cam_kind())
            .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
        let bdf = DeviceFunction {
            bus: bus_nr,
            device: dev_nr,
            function,
        };
        let (root, config) = pci_root.parts_mut();
        pci::support::configure_pci_device_if_needed(root, bdf)
            .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
        let dev_info = root
            .enumerate_bus(bus_nr)
            .find(|(b, _)| *b == bdf)
            .map(|(_, info)| info)
            .ok_or(ProbeError::Fatal(RuntimeError::BadState))?;

        let provider = driver_runtime::resource_provider();
        let (kind, transport, _irq) = probe_pci_device::<
            driver_runtime::hal::Hal,
            pci::MmioCam<'static>,
        >(root, bdf, &dev_info, config, provider)
        .ok_or(ProbeError::Fatal(RuntimeError::BadState))?;
        if !matches!(kind, driver_base::DeviceKind::Char) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        let device = VirtIoRngDev::<driver_runtime::hal::Hal, _>::try_new(
            transport,
            driver_runtime::time_ops(),
        )
        .map_err(map_driver_error)?;

        let location = legacy_location(dev.descriptor().location);
        publish_char(
            ClassParent::detached("virtio-rng", location),
            Box::new(device),
        )
        .map_err(map_driver_error)?;

        Ok(RngData)
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PciBus, RngData>) {
        // Class publication is dropped by the composition root's remove
        // notifier.
    }
}

fn legacy_location(location: driver_core::desc::DeviceLocation) -> kdevice::DeviceLocation {
    use driver_core::desc::DeviceLocation as Ours;
    use kdevice::DeviceLocation as Legacy;
    match location {
        Ours::Pci {
            segment,
            bus,
            device,
            function,
        } => Legacy::Pci {
            segment,
            bus,
            device,
            function,
        },
        Ours::Mmio { base, size } => Legacy::Mmio { base, size },
        Ours::FirmwareNode { id } => Legacy::FirmwareNode { id },
        Ours::PlatformStatic { id } => Legacy::PlatformStatic { id },
        Ours::Bridge { domain } => Legacy::Bridge { domain },
    }
}

fn map_driver_error(err: driver_base::DriverError) -> ProbeError {
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

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new(
        "virtio-rng",
        IDS,
        VirtioRngDriver,
    ));
}
