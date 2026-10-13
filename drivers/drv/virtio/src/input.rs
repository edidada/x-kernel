// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-input driver on the driver-core model (PCI transport).
//!
//! Template application of the PCI consumer pattern: const id table, one
//! class publication, no interrupt ownership in this crate.

use alloc::boxed::Box;

use device_class::{ClassParent, publish_input};
use driver_core::{BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError};
use pci::{PciBus, PciMatch};

/// `struct pci_device_id virtio_input_id_table[]`.
static IDS: &[PciMatch] = &[PciMatch::device(0x1af4, 0x1052)];

/// Driver-private data: the published device is owned by its class registry.
pub struct InputData {
    /// The Data-owned publication: dropping unpublishes (first field).
    _publication: device_class::ClassWithdraw,
}

/// The driver ops.
pub struct InputDriver;

impl DriverOps<PciBus> for InputDriver {
    type Data = InputData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PciBus>) -> Result<InputData, ProbeError> {
        let (kind, transport, irq) = crate::common::pci_transport(dev)?;
        if !matches!(kind, driver_base::DeviceKind::Input) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }
        let publication = activate(dev, transport, irq.unwrap_or(0), None)?;
        Ok(InputData {
            _publication: publication,
        })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PciBus, InputData>) {
        // The Data-owned publication guard unpublishes at Data drop.
    }
}

/// Construct and publish the device for a probed transport — shared by the
/// PCI driver above and the virtio-mmio transport driver.
pub fn activate<B: driver_core::Bus>(
    dev: &mut DeviceCtx<'_, B>,
    transport: impl virtio_devices::Transport + 'static,
    irq: usize,
    mmio_regs: Option<driver_core::IoMap>,
) -> Result<device_class::ClassWithdraw, ProbeError> {
    let device = virtio_devices::VirtIoInputDev::<driver_host::virtio_hal::Hal, _>::try_new(
        transport, irq, mmio_regs,
    )
    .map_err(crate::common::map_driver_error)?;
    let location = crate::common::physical_path(dev);
    let handle = publish_input(
        ClassParent::detached(dev.id().raw(), "virtio-input", location),
        Box::new(device),
    )
    .map_err(crate::common::map_driver_error)?;
    Ok(device_class::input_withdraw(&handle))
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

    // The vendored input device filters its shared-line handler by the
    // LOGICAL virq (the number the host IRQ core dispatches). Resolve the
    // descriptor's irq through the host service — a firmware route (the
    // shape DT enumeration produces) maps through its controller's domain
    // here; reading raw fields would confuse hwirq with virq on every
    // non-identity domain, and rejecting routes broke the only real
    // firmware-enumerated path.
    let irq = dev.resolve_irq(0)?;
    let publication = activate(dev, transport, irq, mmio_regs)?;

    struct InputMmioActivation {
        _withdraw: device_class::ClassWithdraw,
    }

    impl crate::mmio::MmioActivation for InputMmioActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            // Field order sequences the unpublish before later guards.
        }
    }

    Ok(builder.build(InputMmioActivation {
        _withdraw: publication,
    }))
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new("virtio-input", IDS, InputDriver));
}
