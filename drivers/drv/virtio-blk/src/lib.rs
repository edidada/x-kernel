// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-blk driver on the driver-core model (PCI transport).
//!
//! The Phase 1 golden template for **PCI consumer drivers with class
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

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, string::String, sync::Arc};

use block::{
    Gendisk,
    dispatch::{irq as block_irq, reclaimer::BlockIoReclaimer, wait as block_wait},
};
use device_res::{IrqOp, IrqResource, IrqTrigger};
use driver_base::Device as _;
use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, ProbeError, RuntimeError,
    pci::{PciBus, PciMatch},
};
use kclass::{ClassParent, publish_block};
use pci::{DeviceFunction, PciBus as PciRootAccess};
use virtio::{Transport, VirtIoBlkDev, VirtIoHal, probe_pci_device};

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
        use driver_core::desc::DeviceLocation;

        let DeviceLocation::Pci {
            bus: bus_nr,
            device: dev_nr,
            function,
            ..
        } = dev.descriptor().location
        else {
            return Err(ProbeError::Fatal(RuntimeError::Invalid));
        };

        let mut pci_root = PciRootAccess::new(pci::support::pci_cam_kind())
            .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
        let bdf = DeviceFunction {
            bus: bus_nr,
            device: dev_nr,
            function,
        };

        let (root, config) = pci_root.parts_mut();
        pci::support::configure_pci_device_if_needed(root, bdf)
            .map_err(|_| ProbeError::Fatal(RuntimeError::Io))?;
        let dev_info = root
            .enumerate_bus(bus_nr)
            .find(|(b, _)| *b == bdf)
            .map(|(_, info)| info)
            .ok_or(ProbeError::Fatal(RuntimeError::BadState))?;

        let (kind, transport, irq) =
            probe_pci_device::<driver_runtime::hal::Hal, pci::MmioCam<'static>>(
                root,
                bdf,
                &dev_info,
                config,
                driver_runtime::resource_provider(),
            )
            .ok_or(ProbeError::Fatal(RuntimeError::BadState))?;
        if !matches!(kind, driver_base::DeviceKind::Block) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        let activation = activate(dev, transport, irq)?;
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
    irq: Option<device_res::Irq>,
    reclaimer: Arc<BlockIoReclaimer>,
    disk: Option<Arc<Gendisk>>,
}

impl<H: VirtIoHal, T: Transport> Drop for BlkActivation<H, T> {
    fn drop(&mut self) {
        let mut drain = block_wait::prepare_block_wait().expect("block close task context");
        let mut stop = block_wait::prepare_block_wait().expect("block close task context");
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
    irq: usize,
) -> Result<Box<dyn Activation>, ProbeError> {
    let provider: &'static dyn IrqOp = driver_runtime::resource_provider();
    let device = Arc::new(
        VirtIoBlkDev::<driver_runtime::hal::Hal, T>::try_new_irq(
            transport,
            block_wait::prepare_block_wait,
        )
        .map_err(map_driver_error)?,
    );

    let completion: Arc<dyn block::completion::BlockCompletionOperations> = device.clone();
    let reclaimer = BlockIoReclaimer::new(Arc::downgrade(&completion));
    let mut activation = BlkActivation {
        device: device.clone(),
        irq: None,
        reclaimer,
        disk: None,
    };

    let handler: Arc<dyn device_res::IrqHandler> = device.clone();
    activation.irq = Some(
        block_irq::request_block_irq(
            provider,
            IrqResource::new(irq, IrqTrigger::Unknown(0)),
            Arc::downgrade(&handler),
            activation.reclaimer.clone(),
        )
        .map_err(map_res_error)?,
    );

    let modalias = dev.modalias().map(alloc::string::String::from);
    let parent_path = physical_path(dev.descriptor().location);
    let disk = Arc::new(
        Gendisk::new(
            device.name().into(),
            virtio::VIRTIO_BLK_MAJOR,
            device.index() << virtio::VIRTIO_BLK_PART_BITS,
            1 << virtio::VIRTIO_BLK_PART_BITS,
            Box::new(device.clone()),
        )
        .map_err(map_driver_error)?
        .with_devnode_context(&parent_path, modalias),
    );
    activation.disk = Some(disk.clone());

    device.enable_interrupts();

    let location = legacy_location(dev.descriptor().location);
    if let Err(error) = publish_block(ClassParent::detached("virtio-blk", location), disk) {
        return Err(map_driver_error(error));
    }

    Ok(Box::new(activation))
}

fn physical_path(location: driver_core::desc::DeviceLocation) -> String {
    use driver_core::desc::DeviceLocation as Ours;
    match location {
        Ours::Pci {
            segment,
            bus,
            device,
            function,
        } => devnode::physical_path_pci(segment, bus, device, function),
        Ours::Mmio { base, .. } => alloc::format!("platform/{base:x}.mmio"),
        Ours::FirmwareNode { id } => alloc::format!("platform/firmware{id}"),
        Ours::PlatformStatic { id } => alloc::format!("platform/static{id}"),
        Ours::Bridge { domain } => alloc::format!("pci{domain:04x}:00"),
    }
}

fn legacy_location(location: driver_core::desc::DeviceLocation) -> devnode::DeviceLocation {
    use devnode::DeviceLocation as Legacy;
    use driver_core::desc::DeviceLocation as Ours;
    match location {
        Ours::Pci {
            segment,
            bus,
            device,
            function,
        } => Legacy::Pci {
            segment,
            bus,
            device,
            function,
        },
        Ours::Mmio { base, size } => Legacy::Mmio { base, size },
        Ours::FirmwareNode { id } => Legacy::FirmwareNode { id },
        Ours::PlatformStatic { id } => Legacy::PlatformStatic { id },
        Ours::Bridge { domain } => Legacy::Bridge { domain },
    }
}

fn map_driver_error(err: driver_base::DriverError) -> ProbeError {
    let error = match err {
        driver_base::DriverError::InvalidInput | driver_base::DriverError::ReadOnly => {
            RuntimeError::Invalid
        }
        driver_base::DriverError::NoMemory => RuntimeError::NoMemory,
        driver_base::DriverError::ResourceBusy | driver_base::DriverError::AlreadyExists => {
            RuntimeError::Busy
        }
        driver_base::DriverError::BadState => RuntimeError::BadState,
        driver_base::DriverError::Io | driver_base::DriverError::WouldBlock => RuntimeError::Io,
        driver_base::DriverError::Unsupported => RuntimeError::Unsupported,
    };
    ProbeError::Fatal(error)
}

fn map_res_error(err: device_res::ResError) -> ProbeError {
    let error = match err {
        device_res::ResError::InvalidResource => RuntimeError::Invalid,
        device_res::ResError::MappingFailed => RuntimeError::Io,
        device_res::ResError::NoMemory => RuntimeError::NoMemory,
        device_res::ResError::Busy => RuntimeError::Busy,
        device_res::ResError::Unsupported => RuntimeError::Unsupported,
    };
    ProbeError::Fatal(error)
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_core::global_domain().register_driver(DriverDesc::new(
        "virtio-blk",
        IDS,
        VirtioBlkDriver,
    ));
}
