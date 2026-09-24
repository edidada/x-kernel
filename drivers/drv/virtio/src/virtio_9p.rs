// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-9p driver on the driver-core model (PCI transport).
//!
//! Template application of the PCI consumer pattern: const id table, one
//! class publication, no interrupt ownership in this crate.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;

use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError,
    pci::{PciBus, PciMatch},
};
use kclass::{ClassParent, publish_virtio_9p};
use pci::{DeviceFunction, PciBus as PciRootAccess};
use virtio::probe_pci_device;

/// `struct pci_device_id virtio_9p_id_table[]`.
static IDS: &[PciMatch] = &[
    PciMatch::device(0x1af4, 0x1009), // transitional
    PciMatch::device(0x1af4, 0x1049), // modern
];

/// Driver-private data: the published device is owned by its class registry.
pub struct NinepData;

/// The driver ops.
pub struct NinepDriver;

impl DriverOps<PciBus> for NinepDriver {
    type Data = NinepData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PciBus>) -> Result<NinepData, ProbeError> {
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
        if !matches!(kind, driver_base::DeviceKind::Fs9p) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        activate(dev, transport)?;
        Ok(NinepData)
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PciBus, NinepData>) {
        // Class publication is dropped by the composition root's remove
        // notifier.
    }
}

/// Construct and publish the device for a probed transport — shared by the
/// PCI driver above and the virtio-mmio transport driver.
pub fn activate<B: driver_core::Bus>(
    dev: &mut DeviceCtx<'_, B>,
    transport: impl virtio::Transport + 'static,
) -> Result<(), ProbeError> {
    let device = virtio::VirtIo9pDev::<driver_runtime::hal::Hal, _>::try_new(transport)
        .map_err(map_driver_error)?;
    let location = legacy_location(dev.descriptor().location);
    publish_virtio_9p(
        ClassParent::detached("virtio-9p", location),
        Box::new(device),
    )
    .map_err(map_driver_error)?;
    Ok(())
}

fn legacy_location(location: driver_core::desc::DeviceLocation) -> devnode::DeviceLocation {
    use devnode::DeviceLocation as Legacy;
    use driver_core::desc::DeviceLocation as Ours;
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
    driver_core::global_domain().register_driver(DriverDesc::new("virtio-9p", IDS, NinepDriver));
}
