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
};
use kclass::{ClassParent, publish_net};
use pci::{PciBus, PciMatch};
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
        let (kind, transport, msi_irq) = crate::common::pci_transport(dev)?;
        if !matches!(kind, driver_base::DeviceKind::Net) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        let irq = activate(dev, transport, msi_irq)?;
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
    msi_irq: Option<usize>,
) -> Result<Option<DevIrq>, ProbeError> {
    // The PCI path's publication is withdrawn by the composition root's
    // remove notifier (before Data drops — the domain ordering contract).
    let (_publication, irq) = activate_with_irq(dev, transport, msi_irq)?;
    Ok(irq)
}

/// Construct and publish the NIC, returning the publication handle and the
/// registered IRQ guard (shared by the PCI driver and the mmio transport).
fn activate_with_irq<B: driver_core::Bus, T: virtio::Transport + 'static>(
    dev: &mut DeviceCtx<'_, B>,
    transport: T,
    msi_irq: Option<usize>,
) -> Result<(kclass::ClassDevice<kclass::NetDeviceImpl>, Option<DevIrq>), ProbeError> {
    let device = Arc::new(
        VirtIoNetDev::<driver_runtime::hal::Hal, _, 64>::try_new(transport)
            .map_err(crate::common::map_driver_error)?,
    );
    let worker = device.irq_worker();
    // The worker holds only shared internals; the publication owns the
    // device exclusively.
    let device = Arc::try_unwrap(device).map_err(|_| ProbeError::Fatal(RuntimeError::BadState))?;

    // Interrupt ownership: the handler holds only a share of the device
    // internals; the guard releases when the activation closes. The
    // transport-bound vector comes first: when MSI-X was set up the device
    // interrupts ONLY on that vector (prepare_msix disabled legacy INTx),
    // so a handler on the descriptor's legacy line would never fire.
    let irq = match msi_irq.map(|number| {
        dev.request_irq(
            driver_core::IrqSpec::global(number, driver_core::IrqTrigger::Unknown),
            "virtio-net",
            {
                let worker = worker.clone();
                move || {
                    if worker.service() {
                        IrqAction::Handled
                    } else {
                        IrqAction::NotHandled
                    }
                }
            },
        )
    }) {
        Some(Ok(irq)) => Some(irq),
        Some(Err(err)) => return Err(err),
        // No MSI-X vector (aarch64 legacy INTx): the descriptor's route
        // resolves through the controller's irq domain.
        None => match dev.irq(0) {
            Ok(spec) => Some(dev.request_irq(spec, "virtio-net", {
                let worker = worker.clone();
                move || {
                    if worker.service() {
                        IrqAction::Handled
                    } else {
                        IrqAction::NotHandled
                    }
                }
            })?),
            Err(_) => None,
        },
    };
    let location = crate::common::legacy_location(dev.descriptor().location);
    let handle = publish_net(
        ClassParent::detached(dev.id(), "virtio-net", location),
        Box::new(device),
    )
    .map_err(crate::common::map_driver_error)?;
    Ok((handle, irq))
}

/// virtio-mmio activation: owns the class publication and the IRQ guard;
/// close() unpublishes first (consumers stop), then releases the IRQ, so
/// nothing dereferences the transport after the mapping drops.
#[cfg(feature = "mmio")]
pub fn activate_mmio<B: driver_core::Bus, T: virtio::Transport + 'static>(
    dev: &mut DeviceCtx<'_, B>,
    transport: T,
    builder: crate::mmio::ActivationBuilder,
) -> Result<crate::mmio::MmioData, ProbeError> {
    // virtio-mmio has no MSI-X vector: legacy route only.
    let (publication, irq) = activate_with_irq(dev, transport, None)?;
    struct NetMmioActivation {
        publication: kclass::ClassDevice<kclass::NetDeviceImpl>,
        irq: Option<DevIrq>,
    }
    impl crate::mmio::MmioActivation for NetMmioActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            kclass::unpublish_net(self.publication.id());
            drop(self.irq);
        }
    }
    Ok(builder.build(NetMmioActivation { publication, irq }))
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new(
        "virtio-net",
        IDS,
        VirtioNetDriver,
    ));
}
