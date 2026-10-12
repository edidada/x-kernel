// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-9p driver on the driver-core model (PCI transport).
//!
//! Template application of the PCI consumer pattern: const id table, one
//! class publication, no interrupt ownership in this crate.

use alloc::boxed::Box;

use device_class::{ClassParent, publish_virtio_9p};
use driver_core::{BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError};
use pci::{PciBus, PciMatch};

/// `struct pci_device_id virtio_9p_id_table[]`.
static IDS: &[PciMatch] = &[
    PciMatch::device(0x1af4, 0x1009), // transitional
    PciMatch::device(0x1af4, 0x1049), // modern
];

/// Driver-private data: the published device is owned by its class registry.
pub struct NinepData {
    /// The Data-owned publication: dropping unpublishes (first field).
    _publication: device_class::ClassWithdraw,
}

/// The driver ops.
pub struct NinepDriver;

impl DriverOps<PciBus> for NinepDriver {
    type Data = NinepData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PciBus>) -> Result<NinepData, ProbeError> {
        let (kind, transport, _irq) = crate::common::pci_transport(dev)?;
        if !matches!(kind, device_traits::DeviceKind::Fs9p) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        let publication = activate(dev, transport, None)?;
        Ok(NinepData {
            _publication: publication,
        })
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
    transport: impl virtio_devices::Transport + 'static,
    mmio_regs: Option<driver_core::IoMap>,
) -> Result<device_class::ClassWithdraw, ProbeError> {
    let device = virtio_devices::VirtIo9pDev::<driver_host::virtio_hal::Hal, _>::try_new(
        transport, mmio_regs,
    )
    .map_err(crate::common::map_driver_error)?;
    let location = dev.descriptor().location;
    let handle = publish_virtio_9p(
        ClassParent::detached(dev.id().raw(), "virtio-9p", location),
        Box::new(device),
    )
    .map_err(crate::common::map_driver_error)?;
    // The Data-owned publication: dropping the guard unpublishes (the
    // single-owner contract — no composition-root class drain at remove).
    Ok(device_class::virtio_9p_withdraw(&handle))
}

/// virtio-mmio activation: owns the class publication; close() unpublishes
/// so nothing dereferences the transport after the mapping drops.
#[cfg(feature = "mmio")]
pub fn activate_mmio(
    dev: &mut driver_core::DeviceCtx<'_, driver_core::platform::PlatformBus>,
    transport: impl virtio_devices::Transport + 'static,
    mut builder: crate::mmio::ActivationBuilder,
) -> Result<crate::mmio::MmioData, ProbeError> {
    // The MMIO register mapping follows the device object (Linux BAR
    // model): released with the last ClassDevice clone.
    let mmio_regs = builder.take_regs();
    let publication = activate(dev, transport, mmio_regs)?;

    struct Virtio9pMmioActivation {
        _withdraw: device_class::ClassWithdraw,
    }

    impl crate::mmio::MmioActivation for Virtio9pMmioActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            // Field order sequences the unpublish before later guards.
        }
    }

    Ok(builder.build(Virtio9pMmioActivation {
        _withdraw: publication,
    }))
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new("virtio-9p", IDS, NinepDriver));
}
