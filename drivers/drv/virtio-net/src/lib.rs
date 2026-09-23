// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-net driver on the driver-core model (PCI transport).
//!
//! The PCI consumer template applied to the network class: const id table,
//! per-device interrupt ownership, and class publication through a detached
//! parent. The interrupt handler owns only a [`VirtIoNetIrqWorker`] — a
//! share of the device's internals — so IRQ delivery is bound to *this*
//! device at registration time. The legacy `NET_IRQ_HANDLES` routing table
//! is gone from the device crate entirely.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, sync::Arc};

use driver_core::{
    BoundDevice, DevIrq, DeviceCtx, DriverDesc, DriverOps, IrqAction, ProbeError, RuntimeError,
    pci::{PciBus, PciMatch},
};
use kclass::{ClassParent, publish_net};
use pci::{DeviceFunction, PciBus as PciRootAccess};
use virtio::{VirtIoNetDev, probe_pci_device};

/// `struct pci_device_id virtio_net_id_table[]`: transitional 0x1000 and
/// modern 0x1041.
static IDS: &[PciMatch] = &[
    PciMatch::device(0x1af4, 0x1000),
    PciMatch::device(0x1af4, 0x1041),
];

/// Driver-private data: the interrupt guard. The published network device
/// itself is owned by the net class registry.
pub struct NetData {
    _irq: Option<DevIrq>,
}

/// The driver ops.
pub struct VirtioNetDriver;

impl DriverOps<PciBus> for VirtioNetDriver {
    type Data = NetData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PciBus>) -> Result<NetData, ProbeError> {
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
        let (kind, transport, _legacy_irq) = probe_pci_device::<
            driver_runtime::hal::Hal,
            pci::MmioCam<'static>,
        >(root, bdf, &dev_info, config, provider)
        .ok_or(ProbeError::Fatal(RuntimeError::BadState))?;
        if !matches!(kind, driver_base::DeviceKind::Net) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        let device = Arc::new(
            VirtIoNetDev::<driver_runtime::hal::Hal, _, 64>::try_new(transport)
                .map_err(map_driver_error)?,
        );
        let worker = device.irq_worker();
        // The worker holds only shared internals; the publication owns the
        // device exclusively.
        let device =
            Arc::try_unwrap(device).map_err(|_| ProbeError::Fatal(RuntimeError::BadState))?;

        // Interrupt ownership: the handler holds only a share of the device
        // internals; the guard in `Data` releases it when the device goes.
        let irq = match dev.irq(0) {
            Ok(spec) => Some(dev.request_irq(spec, "virtio-net", move || {
                if worker.service() {
                    IrqAction::Handled
                } else {
                    IrqAction::NotHandled
                }
            })?),
            Err(_) => None,
        };
        let location = legacy_location(dev.descriptor().location);
        publish_net(
            ClassParent::detached("virtio-net", location),
            Box::new(device),
        )
        .map_err(map_driver_error)?;

        Ok(NetData { _irq: irq })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PciBus, NetData>) {
        // The class publication is dropped by the composition root's remove
        // notifier; the IRQ guard releases with `Data`.
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
        "virtio-net",
        IDS,
        VirtioNetDriver,
    ));
}
