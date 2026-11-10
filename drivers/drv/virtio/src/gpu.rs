// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-gpu driver on the driver-core model (PCI transport).
//!
//! Template application of the PCI consumer pattern: const id table, one
//! class publication, no interrupt ownership in this crate.

use alloc::boxed::Box;

use driver_base::{Device, DriverOps, ProbeError, RuntimeError};
use pci::{PciBus, PciMatch};
use subsys_display::class::DISPLAY;
use virtio_devices::VirtioDeviceKind;

/// `struct pci_device_id virtio_gpu_id_table[]`.
static IDS: &[PciMatch] = &[PciMatch::device(0x1af4, 0x1050)];

/// Driver-private data: the published device is owned by its class registry.
pub struct GpuData {
    /// The Data-owned publication: dropping unpublishes (first field —
    /// runs before any later guard releases).
    _publication: driver_base::Publication<subsys_display::class::DisplayDeviceImpl>,
}

/// The driver ops.
pub struct GpuDriver;

impl DriverOps<PciBus> for GpuDriver {
    type Data = GpuData;

    fn probe(&self, dev: &mut Device<'_, PciBus>) -> Result<GpuData, ProbeError> {
        let (kind, transport, _irq) = crate::common::pci_transport(dev)?;
        if !matches!(kind, VirtioDeviceKind::Display) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        let publication = activate(dev, transport, None)?;
        Ok(GpuData {
            _publication: publication,
        })
    }

    fn remove(&self, _dev: &mut Device<'_, PciBus>, _data: &mut GpuData) {
        // The Data-owned publication guard unpublishes at Data drop
        // (remove returns first, then the framework drops Data).
    }
}

/// Construct and publish the device for a probed transport — shared by the
/// PCI driver above and the virtio-mmio transport driver.
pub fn activate<B: driver_base::Bus>(
    dev: &mut Device<'_, B>,
    transport: impl virtio_devices::Transport + 'static,
    mmio_regs: Option<driver_base::IoMap>,
) -> Result<driver_base::Publication<subsys_display::class::DisplayDeviceImpl>, ProbeError> {
    let device = virtio_devices::VirtIoGpuDev::<driver_host::virtio_hal::Hal, _>::try_new(
        transport, mmio_regs,
    )
    .map_err(crate::common::map_driver_error)?;
    let handle = dev
        .publish(&DISPLAY, Box::new(device), "virtio-gpu")
        .map_err(crate::common::map_driver_error)?;
    // The Data-owned publication: dropping the guard unpublishes (the
    // single-owner contract — no composition-root class drain at remove).
    Ok(handle)
}

/// virtio-mmio activation: owns the class publication; close() unpublishes
/// so nothing dereferences the transport after the mapping drops.
#[cfg(feature = "mmio")]
pub fn activate_mmio(
    dev: &mut driver_base::Device<'_, driver_base::PlatformBus>,
    transport: impl virtio_devices::Transport + 'static,
    mut builder: crate::mmio::ActivationBuilder,
) -> Result<crate::mmio::MmioData, ProbeError> {
    // The MMIO register mapping follows the device object (Linux BAR
    // model): released with the last Publication clone.
    let mmio_regs = builder.take_regs();

    // Publish through the shared constructor and carry the withdrawal
    // guard: the activation's fields drop in declaration order, so the
    // unpublish runs first and the mapping guard releases last.
    let withdraw = activate(dev, transport, mmio_regs)?;

    struct GpuMmioActivation {
        _withdraw: driver_base::Publication<subsys_display::class::DisplayDeviceImpl>,
    }

    impl crate::common::Activation for GpuMmioActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            // Field order does the sequencing: withdraw (unpublish) first.
        }
    }

    Ok(builder.build(GpuMmioActivation {
        _withdraw: withdraw,
    }))
}

driver_base::declare_driver!(PciBus, GpuDriver, "virtio-gpu", IDS);
