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

use device_class::{ClassParent, publish_net};
use driver_base::{
    BoundDevice, DevIrq, DeviceCtx, DriverDesc, DriverOps, IrqAction, ProbeError, RuntimeError,
};
use pci::{PciBus, PciMatch};
use virtio_devices::{VirtIoNetDev, VirtioDeviceKind};

/// `struct pci_device_id virtio_net_id_table[]`: transitional 0x1000 and
/// modern 0x1041.
static IDS: &[PciMatch] = &[
    PciMatch::device(0x1af4, 0x1000),
    PciMatch::device(0x1af4, 0x1041),
];

/// Driver-private data: the interrupt guard. The published network device
/// itself is owned by the net class registry.
pub struct NetData {
    /// The Data-owned publication: dropping unpublishes. FIRST field —
    /// runs before the IRQ guard below releases (consumers detach while
    /// the device is still live).
    _publication: device_class::ClassWithdraw,
    _irq: Option<DevIrq>,
}

/// The driver ops.
pub struct VirtioNetDriver;

impl DriverOps<PciBus> for VirtioNetDriver {
    type Data = NetData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PciBus>) -> Result<NetData, ProbeError> {
        let (kind, transport, msi_irq) = crate::common::pci_transport(dev)?;
        if !matches!(kind, VirtioDeviceKind::Net) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        let (publication, irq) = activate(dev, transport, msi_irq)?;
        Ok(NetData {
            _publication: publication,
            _irq: irq,
        })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PciBus, NetData>) {
        // The class publication is dropped by the composition root's remove
        // notifier; the IRQ guard releases with `Data`.
    }
}

/// Construct and publish the NIC for a probed transport — shared by the PCI
/// driver above and the virtio-mmio transport driver. Returns the registered
/// interrupt guard.
pub fn activate<B: driver_base::Bus>(
    dev: &mut DeviceCtx<'_, B>,
    transport: impl virtio_devices::Transport + 'static,
    msi_irq: Option<usize>,
) -> Result<(device_class::ClassWithdraw, Option<DevIrq>), ProbeError> {
    // The publication rides the caller's Data (withdrawal guard): the
    // Data-owner contract — no composition-root class drain at remove.
    let (publication, irq) = activate_with_irq(dev, transport, msi_irq, None)?;
    let withdraw = device_class::net_withdraw(&publication);
    Ok((withdraw, irq))
}

/// Construct and publish the NIC, returning the publication handle and the
/// registered IRQ guard (shared by the PCI driver and the mmio transport).
fn activate_with_irq<B: driver_base::Bus, T: virtio_devices::Transport + 'static>(
    dev: &mut DeviceCtx<'_, B>,
    transport: T,
    msi_irq: Option<usize>,
    mmio_regs: Option<driver_base::IoMap>,
) -> Result<
    (
        device_class::ClassDevice<device_class::NetDeviceImpl>,
        Option<DevIrq>,
    ),
    ProbeError,
> {
    let device = Arc::new(
        VirtIoNetDev::<driver_host::virtio_hal::Hal, _, 64>::try_new(transport, mmio_regs)
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
            driver_base::IrqSpec::resolved(number, driver_base::IrqTrigger::Unknown(0)),
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
    // The irq-backed flag must reflect reality: only a registered handler
    // lets recv() skip its task-context ack and arms the RX scheduler.
    if irq.is_some() {
        device.mark_irq_backed();
    }
    let location = crate::common::physical_path(dev);
    let handle = publish_net(
        ClassParent::detached(dev.id().raw(), "virtio-net", location),
        Box::new(device),
        "virtio-net",
    )
    .map_err(crate::common::map_driver_error)?;
    Ok((handle, irq))
}

/// virtio-mmio activation: owns the class publication and the IRQ guard;
/// close() unpublishes first (consumers stop), then releases the IRQ, so
/// nothing dereferences the transport after the mapping drops.
#[cfg(feature = "mmio")]
pub fn activate_mmio<B: driver_base::Bus, T: virtio_devices::Transport + 'static>(
    dev: &mut DeviceCtx<'_, B>,
    transport: T,
    mut builder: crate::mmio::ActivationBuilder,
) -> Result<crate::mmio::MmioData, ProbeError> {
    // The MMIO register mapping follows the device object (the Linux BAR
    // model): the last ClassDevice clone referencing this NIC releases it.
    let mmio_regs = builder.take_regs();
    // virtio-mmio has no MSI-X vector: legacy route only.
    let (publication, irq) = activate_with_irq(dev, transport, None, mmio_regs)?;
    struct NetMmioActivation {
        // Field order = teardown order: unpublish (consumers detach while
        // the device is live), then the IRQ guard releases.
        _withdraw: device_class::ClassWithdraw,
        irq: Option<DevIrq>,
    }
    impl crate::mmio::MmioActivation for NetMmioActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            // Field order does the sequencing.
        }
    }
    Ok(builder.build(NetMmioActivation {
        _withdraw: device_class::net_withdraw(&publication),
        irq,
    }))
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_base::global_domain().register_driver(DriverDesc::new(
        "virtio-net",
        IDS,
        VirtioNetDriver,
    ));
}
