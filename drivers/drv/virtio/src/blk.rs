// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-blk driver on the driver-core model (PCI transport).
//!
//! The golden template for **PCI consumer drivers with class
//! publication**: the device is discovered on the PCI bus, matched by a
//! const id table (`struct pci_device_id` → [`IDS`]), and the disk is
//! published into the block class under a detached class parent.
//!
//! Everything the legacy glue needed a global side table for is now owned
//! by [`BlkData`]:
//!
//! * the activation (transport + IRQ + reclaimer + disk) lives in `Data`,
//!   so probe failure rolls it back via `Drop` and device removal closes
//!   it through [`DriverOps::remove`] — no `ACTIVATIONS: BTreeMap` and no
//!   `add_cleanup` closure black box;
//! * the IRQ handler is registered per device and captures the device
//!   through a `Weak` (the `dev_id` pattern);
//! * the completion path reuses the block-class dispatch services
//!   (`block::dispatch`), which are model-agnostic.

use alloc::{boxed::Box, sync::Arc};

use block::{
    Gendisk,
    dispatch::{reclaimer::BlockIoReclaimer, wait as block_wait},
};
use device_class::{ClassParent, publish_block};
use driver_base::Device as _;
use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, IrqAction, ProbeError, RuntimeError,
};
use pci::{PciBus, PciMatch};
use virtio_devices::{Transport, VirtIoBlkDev, VirtIoHal};

/// `struct pci_device_id virtio_blk_id_table[]`: modern 0x1042 and
/// transitional 0x1001. Note the transitional series is type-indexed
/// (0x1000 is *network*) — a wrong row here silently steals another
/// driver's device.
static IDS: &[PciMatch] = &[
    PciMatch::device(0x1af4, 0x1001),
    PciMatch::device(0x1af4, 0x1042),
];

/// Erased activation owned by the device; closing quiesces in order:
/// drain in-flight I/O, unregister the disk, stop IRQ generation, stop the
/// reclaimer, then destroy the transport.
pub trait Activation: Send + Sync {
    fn close(self: Box<Self>);
}

/// Driver-private data (`struct virtio_blk`): the activation, closed by
/// `remove` or by `Drop` on rollback.
pub struct BlkData {
    activation: Option<Box<dyn Activation>>,
}

/// The driver ops.
pub struct VirtioBlkDriver;

impl DriverOps<PciBus> for VirtioBlkDriver {
    type Data = BlkData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PciBus>) -> Result<BlkData, ProbeError> {
        let (kind, transport, irq) = crate::common::pci_transport(dev)?;
        if !matches!(kind, driver_base::DeviceKind::Block) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        // The transport-bound vector is already an OS-visible virq; the
        // descriptor's legacy route (controller+hwirq) resolves through the
        // irq domain. Either way `activate` receives the full spec.
        let spec = irq
            .map(|number| {
                driver_core::IrqSpec::resolved(number, driver_core::IrqTrigger::Unknown(0))
            })
            .or_else(|| dev.irq(0).ok());
        let activation = match spec {
            Some(spec) => activate(dev, transport, spec, None)?,
            None => return Err(ProbeError::Fatal(RuntimeError::Invalid)),
        };
        Ok(BlkData {
            activation: Some(activation),
        })
    }

    fn remove(&self, dev: &mut BoundDevice<'_, PciBus, BlkData>) {
        if let Some(activation) = dev.data_mut().activation.take() {
            activation.close();
        }
    }
}

impl Drop for BlkData {
    fn drop(&mut self) {
        if let Some(activation) = self.activation.take() {
            activation.close();
        }
    }
}

/// Everything the legacy glue kept in the `ACTIVATIONS` global map, now a
/// value owned by the device.
struct BlkActivation<H: VirtIoHal, T: Transport> {
    device: Arc<VirtIoBlkDev<H, T>>,
    irq: Option<driver_core::DevIrq>,
    reclaimer: Arc<BlockIoReclaimer>,
    disk: Option<Arc<Gendisk>>,
    /// The Data-owned publication: taken and dropped at the exact
    /// begin_close → unpublish → drain point in `Drop` (field order alone
    /// cannot express the interleaving).
    publication: Option<device_class::ClassWithdraw>,
}

impl<H: VirtIoHal, T: Transport> Drop for BlkActivation<H, T> {
    fn drop(&mut self) {
        // Remove runs in task context by construction (the device model's
        // teardown owner is the composition root's remove path).
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: remove-path teardown runs in task \
                      context; a wait-source allocation failure here cannot be recovered from \
                      inside Drop"
        )]
        let (mut drain, mut stop) = {
            let drain = block_wait::prepare_block_wait().expect("block close task context");
            let stop = block_wait::prepare_block_wait().expect("block close task context");
            (drain, stop)
        };
        self.device.begin_close(drain.signals());
        if let Some(disk) = &self.disk
            && disk.part0().is_some()
        {
            block::del_gendisk(disk.device_number());
        }
        drain.wait_completion();
        self.device.disable_interrupts();
        drop(self.irq.take());
        self.reclaimer.stop_and_wait(stop.as_mut());
        self.device.finish_close();
    }
}

impl<H: VirtIoHal + 'static, T: Transport + 'static> Activation for BlkActivation<H, T> {
    fn close(self: Box<Self>) {
        drop(self);
    }
}

/// Construct and publish the block device for a probed transport — shared
/// by the PCI driver above and the virtio-mmio transport driver.
pub fn activate<B: driver_core::Bus, T: Transport + 'static>(
    dev: &mut DeviceCtx<'_, B>,
    transport: T,
    irq: driver_core::IrqSpec,
    mmio_regs: Option<driver_core::IoMap>,
) -> Result<Box<dyn Activation>, ProbeError> {
    let device = Arc::new(
        VirtIoBlkDev::<driver_host::virtio_hal::Hal, T>::try_new_irq_with_regs(
            transport,
            block_wait::prepare_block_wait,
            mmio_regs,
        )
        .map_err(crate::common::map_driver_error)?,
    );

    let completion: Arc<dyn block::completion::BlockCompletionOperations> = device.clone();
    let reclaimer = BlockIoReclaimer::new(Arc::downgrade(&completion));
    let mut activation = BlkActivation {
        publication: None,
        device: device.clone(),
        irq: None,
        reclaimer,
        disk: None,
    };

    // One IRQ entry point for every driver: DeviceCtx::request_irq (the
    // same services path virtio-net takes). The handler keeps only a weak
    // device reference (the activation owns the strong one and withdraws
    // the irq before teardown) and marks the reclaimer for deferred I/O
    // reclaiming when the device claimed the event. `ack_irq` is the family
    // library's inherent acknowledgement method — the registration itself is
    // the framework's, which is what keeps the DevIrq guard in `Data`.
    let weak_device = Arc::downgrade(&device);
    let weak_reclaimer = alloc::sync::Arc::downgrade(&activation.reclaimer);
    activation.irq = Some(dev.request_irq(irq, "virtio-blk", move || {
        let Some(device) = weak_device.upgrade() else {
            return IrqAction::NotHandled;
        };
        if device.ack_irq(0) {
            if let Some(reclaimer) = weak_reclaimer.upgrade() {
                reclaimer.mark_pending();
            }
            IrqAction::Handled
        } else {
            IrqAction::NotHandled
        }
    })?);

    let modalias = dev.modalias().map(alloc::string::String::from);
    let parent_path = crate::common::physical_path(dev);
    let disk = Arc::new(
        Gendisk::new(
            device.name().into(),
            virtio_devices::VIRTIO_BLK_MAJOR,
            device.index() << virtio_devices::VIRTIO_BLK_PART_BITS,
            1 << virtio_devices::VIRTIO_BLK_PART_BITS,
            Box::new(device.clone()),
        )
        .map_err(crate::common::map_driver_error)?
        .with_devnode_context(&parent_path, modalias),
    );
    activation.disk = Some(disk.clone());

    device.enable_interrupts();

    let location = crate::common::physical_path(dev);
    let publication = publish_block(
        ClassParent::detached(dev.id().raw(), "virtio-blk", location),
        disk,
    )
    .map_err(crate::common::map_driver_error)?;
    activation.publication = Some(device_class::block_withdraw(&publication));

    Ok(Box::new(activation))
}

/// virtio-mmio activation: owns the block activation (which owns the
/// Gendisk publication + IRQ) through the same `Activation` close path;
/// the mapping drops after close returns, so the transport never outlives
/// its window.
#[cfg(feature = "mmio")]
pub fn activate_mmio<B: driver_core::Bus, T: Transport + 'static>(
    dev: &mut DeviceCtx<'_, B>,
    transport: T,
    irq: driver_core::IrqSpec,
    mut builder: crate::mmio::ActivationBuilder,
) -> Result<crate::mmio::MmioData, ProbeError> {
    // The MMIO register mapping follows the device object (Linux BAR
    // model): released when the last ClassDevice clone drops the disk.
    let mmio_regs = builder.take_regs();
    let activation = activate(dev, transport, irq, mmio_regs)?;
    struct BlkMmioActivation {
        inner: Box<dyn Activation>,
    }
    impl crate::mmio::MmioActivation for BlkMmioActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            // Activation::close already runs the Linux order: drain I/O,
            // del_gendisk, disable IRQs, release guards.
            self.inner.close();
        }
    }
    Ok(builder.build(BlkMmioActivation { inner: activation }))
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new(
        "virtio-blk",
        IDS,
        VirtioBlkDriver,
    ));
}
