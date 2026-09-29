// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-rng driver on the driver-core model (PCI transport).
//!
//! The smallest consumer template: const PCI id table, one class
//! publication (`char`), no interrupt. The entropy subsystem consumes it
//! through the model-agnostic `kclass::char_devices()` view.

use alloc::boxed::Box;

use driver_core::{BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError};
use kclass::{ClassParent, publish_char};
use pci::{PciBus, PciMatch};
use virtio::VirtIoRngDev;

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
        let (kind, transport, _irq) = crate::common::pci_transport(dev)?;
        if !matches!(kind, driver_base::DeviceKind::Char) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        activate(dev, transport)?;
        Ok(RngData)
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PciBus, RngData>) {
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
    let device =
        VirtIoRngDev::<driver_runtime::hal::Hal, _>::try_new(transport, driver_runtime::time_ops())
            .map_err(crate::common::map_driver_error)?;
    let location = crate::common::legacy_location(dev.descriptor().location);
    publish_char(
        ClassParent::detached(dev.id(), "virtio-rng", location),
        Box::new(device),
    )
    .map_err(crate::common::map_driver_error)?;
    Ok(())
}

/// virtio-mmio activation: owns the class publication; close() unpublishes
/// so nothing dereferences the transport after the mapping drops.
#[cfg(feature = "mmio")]
pub fn activate_mmio<T: virtio::Transport + 'static>(
    owner: u64,
    location: devnode::DeviceLocation,
    transport: T,
    builder: crate::mmio::ActivationBuilder,
) -> Result<crate::mmio::MmioData, ProbeError> {
    let device =
        VirtIoRngDev::<driver_runtime::hal::Hal, _>::try_new(transport, driver_runtime::time_ops())
            .map_err(crate::common::map_driver_error)?;
    let handle = publish_char(
        ClassParent::detached(owner, "virtio-rng", location),
        Box::new(device),
    )
    .map_err(crate::common::map_driver_error)?;

    struct RngActivation {
        publication: kclass::ClassDevice<kclass::CharDeviceImpl>,
    }

    impl crate::mmio::MmioActivation for RngActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            kclass::unpublish_char(self.publication.id());
        }
    }

    Ok(builder.build(RngActivation {
        publication: handle,
    }))
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new(
        "virtio-rng",
        IDS,
        VirtioRngDriver,
    ));
}
