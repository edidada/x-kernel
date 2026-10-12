// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-vsock driver on the driver-core model (PCI transport).
//!
//! Template application of the PCI consumer pattern: const id table, one
//! class publication, no interrupt ownership in this crate.

use alloc::boxed::Box;

use device_class::{ClassParent, publish_vsock};
use driver_core::{BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError};
use pci::{PciBus, PciMatch};

/// `struct pci_device_id virtio_vsock_id_table[]`.
static IDS: &[PciMatch] = &[
    PciMatch::device(0x1af4, 0x1012), // transitional
    PciMatch::device(0x1af4, 0x1053), // modern
];

/// Driver-private data: the published device is owned by its class registry.
pub struct VsockData {
    /// The Data-owned publication: dropping unpublishes (first field).
    _publication: device_class::ClassWithdraw,
}

/// The driver ops.
pub struct VsockDriver;

impl DriverOps<PciBus> for VsockDriver {
    type Data = VsockData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PciBus>) -> Result<VsockData, ProbeError> {
        let (kind, transport, _irq) = crate::common::pci_transport(dev)?;
        if !matches!(kind, device_traits::DeviceKind::Vsock) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        let publication = activate(dev, transport, None)?;
        Ok(VsockData {
            _publication: publication,
        })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PciBus, VsockData>) {
        // The Data-owned publication guard unpublishes at Data drop.
    }
}

/// Construct and publish the device for a probed transport — shared by the
/// PCI driver above and the virtio-mmio transport driver.
pub fn activate<B: driver_core::Bus>(
    dev: &mut DeviceCtx<'_, B>,
    transport: impl virtio_devices::Transport + 'static,
    mmio_regs: Option<driver_core::IoMap>,
) -> Result<device_class::ClassWithdraw, ProbeError> {
    let device = virtio_devices::VirtIoVsockDev::<driver_host::virtio_hal::Hal, _>::try_new(
        transport, mmio_regs,
    )
    .map_err(crate::common::map_driver_error)?;
    let location = dev.descriptor().location;
    let handle = publish_vsock(
        ClassParent::detached(dev.id().raw(), "virtio-vsock", location),
        Box::new(device),
    )
    .map_err(crate::common::map_driver_error)?;
    Ok(device_class::vsock_withdraw(&handle))
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

    struct VsockMmioActivation {
        _withdraw: device_class::ClassWithdraw,
    }

    impl crate::mmio::MmioActivation for VsockMmioActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            // Field order sequences the unpublish before later guards.
        }
    }

    Ok(builder.build(VsockMmioActivation {
        _withdraw: publication,
    }))
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new("virtio-vsock", IDS, VsockDriver));
}
