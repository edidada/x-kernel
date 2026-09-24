// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-gpu driver on the driver-core model (PCI transport).
//!
//! Template application of the PCI consumer pattern: const id table, one
//! class publication, no interrupt ownership in this crate.

use alloc::boxed::Box;

use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError,
    pci::{PciBus, PciMatch},
};
use kclass::{ClassParent, publish_display};

/// `struct pci_device_id virtio_gpu_id_table[]`.
static IDS: &[PciMatch] = &[
    PciMatch::device(0x1af4, 0x1000),
    PciMatch::device(0x1af4, 0x1050),
];

/// Driver-private data: the published device is owned by its class registry.
pub struct GpuData;

/// The driver ops.
pub struct GpuDriver;

impl DriverOps<PciBus> for GpuDriver {
    type Data = GpuData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PciBus>) -> Result<GpuData, ProbeError> {
        let (kind, transport, _irq) = dev.virtio_pci_transport_as::<virtio::PciTransport>()?;
        if !matches!(kind, driver_base::DeviceKind::Display) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        activate(dev, transport)?;
        Ok(GpuData)
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PciBus, GpuData>) {
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
    let device = virtio::VirtIoGpuDev::<driver_runtime::hal::Hal, _>::try_new(transport)
        .map_err(crate::common::map_driver_error)?;
    let location = crate::common::legacy_location(dev.descriptor().location);
    publish_display(
        ClassParent::detached("virtio-gpu", location),
        Box::new(device),
    )
    .map_err(crate::common::map_driver_error)?;
    Ok(())
}

/// virtio-mmio activation: owns the class publication; close() unpublishes
/// so nothing dereferences the transport after the mapping drops.
#[cfg(feature = "mmio")]
pub fn activate_mmio<T: virtio::Transport + 'static>(
    location: devnode::DeviceLocation,
    transport: T,
    builder: crate::mmio::ActivationBuilder,
) -> Result<crate::mmio::MmioData, ProbeError> {
    let device = virtio::VirtIoGpuDev::<driver_runtime::hal::Hal, _>::try_new(transport)
        .map_err(crate::common::map_driver_error)?;
    let handle = publish_display(
        ClassParent::detached("virtio-gpu", location),
        Box::new(device),
    )
    .map_err(crate::common::map_driver_error)?;

    struct GpuMmioActivation {
        publication: kclass::ClassDevice<kclass::DisplayDeviceImpl>,
    }

    impl crate::mmio::MmioActivation for GpuMmioActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            kclass::unpublish_display(self.publication.id());
        }
    }

    Ok(builder.build(GpuMmioActivation {
        publication: handle,
    }))
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new("virtio-gpu", IDS, GpuDriver));
}
