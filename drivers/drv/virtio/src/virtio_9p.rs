// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-9p driver on the driver-core model (PCI transport).
//!
//! Template application of the PCI consumer pattern: const id table, one
//! class publication, no interrupt ownership in this crate.

use alloc::boxed::Box;

use driver_core::{BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError};
use kclass::{ClassParent, publish_virtio_9p};
use pci::{PciBus, PciMatch};

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
        let (kind, transport, _irq) = crate::common::pci_transport(dev)?;
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
        .map_err(crate::common::map_driver_error)?;
    let location = dev.descriptor().location;
    publish_virtio_9p(
        ClassParent::detached(dev.id().raw(), "virtio-9p", location),
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
    let device = virtio::VirtIo9pDev::<driver_runtime::hal::Hal, _>::try_new(transport)
        .map_err(crate::common::map_driver_error)?;
    let handle = publish_virtio_9p(
        ClassParent::detached(owner, "virtio-9p", location),
        Box::new(device),
    )
    .map_err(crate::common::map_driver_error)?;

    struct Virtio9pMmioActivation {
        publication: kclass::ClassDevice<kclass::Virtio9pDeviceImpl>,
    }

    impl crate::mmio::MmioActivation for Virtio9pMmioActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            kclass::unpublish_virtio_9p(self.publication.id());
        }
    }

    Ok(builder.build(Virtio9pMmioActivation {
        publication: handle,
    }))
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new("virtio-9p", IDS, NinepDriver));
}
