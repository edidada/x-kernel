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

use alloc::{boxed::Box, sync::Arc};

use driver_core::{
    BoundDevice, DevIrq, DeviceCtx, DriverDesc, DriverOps, IrqAction, ProbeError, RuntimeError,
    pci::{PciBus, PciMatch},
};
use kclass::{ClassParent, publish_net};
use virtio::VirtIoNetDev;

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
        let (kind, transport, _irq) = dev.virtio_pci_transport_as::<virtio::PciTransport>()?;
        if !matches!(kind, driver_base::DeviceKind::Net) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        let irq = activate(dev, transport)?;
        Ok(NetData { _irq: irq })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PciBus, NetData>) {
        // The class publication is dropped by the composition root's remove
        // notifier; the IRQ guard releases with `Data`.
    }
}

/// Construct and publish the NIC for a probed transport — shared by the PCI
/// driver above and the virtio-mmio transport driver. Returns the registered
/// interrupt guard.
pub fn activate<B: driver_core::Bus>(
    dev: &mut DeviceCtx<'_, B>,
    transport: impl virtio::Transport + 'static,
) -> Result<Option<DevIrq>, ProbeError> {
    let device = Arc::new(
        VirtIoNetDev::<driver_runtime::hal::Hal, _, 64>::try_new(transport)
            .map_err(crate::common::map_driver_error)?,
    );
    let worker = device.irq_worker();
    // The worker holds only shared internals; the publication owns the
    // device exclusively.
    let device = Arc::try_unwrap(device).map_err(|_| ProbeError::Fatal(RuntimeError::BadState))?;

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
    let location = crate::common::legacy_location(dev.descriptor().location);
    publish_net(
        ClassParent::detached("virtio-net", location),
        Box::new(device),
    )
    .map_err(crate::common::map_driver_error)?;
    Ok(irq)
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new(
        "virtio-net",
        IDS,
        VirtioNetDriver,
    ));
}
