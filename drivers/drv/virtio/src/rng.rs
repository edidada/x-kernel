// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! virtio-rng driver on the driver-core model.
//!
//! The device collects entropy IRQ-driven (main's !916 semantics): reads
//! sleep on the host [`RngCompletion`] capability until the queue interrupt
//! delivers data. Published through the `hwrng` class; the entropy
//! subsystem's VirtIO source consumes it when `KFEAT_ENTROPY_VIRTIO_RNG`
//! connects the class to the pool (a separate trust decision).

use alloc::sync::{Arc, Weak};

use device_res::IrqHandler;
use driver_base::DriverError;
use driver_core::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, IrqAction, ProbeError, RuntimeError,
};
use hwrng::completion::RngCompletion;
use kclass::{ClassParent, publish_hwrng};
use kpoll::{PollRegisterError, PollSet};
use ktask::future::PreparedTaskWait;
use pci::{PciBus, PciMatch};
use virtio::VirtIoRngDev;

/// `struct pci_device_id virtio_rng_id_table[]`: transitional 0x1005 and
/// modern 0x1044 (0x1040 + entropy device type 4).
static IDS: &[PciMatch] = &[
    PciMatch::device(0x1af4, 0x1005),
    PciMatch::device(0x1af4, 0x1044),
];

/// Driver-private data: the completion IRQ guard. The removal order is
/// driver.remove() (quiesce) → the composition root's remove notifier
/// withdraws the hwrng publication → drop(Data) releases the IRQ guard.
pub struct RngData {
    _irq: driver_core::DevIrq,
}

/// The driver ops.
pub struct VirtioRngDriver;

impl DriverOps<PciBus> for VirtioRngDriver {
    type Data = RngData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PciBus>) -> Result<RngData, ProbeError> {
        let (kind, transport, _irq) = crate::common::pci_transport(dev)?;
        if !matches!(kind, driver_base::DeviceKind::Hwrng) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        // The hwrng registry owns the publication; the composition root's
        // remove notifier withdraws it when this device goes away.
        let (_publication, irq) = activate(dev, transport)?;
        Ok(RngData { _irq: irq })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PciBus, RngData>) {
        // Class publication is dropped by the composition root's remove
        // notifier.
    }
}

/// The host completion capability (main's `rng_completion` host provider):
/// task-context wait on a PollSet, IRQ-safe wake.
struct HostRngCompletion {
    waiters: PollSet,
}

impl HostRngCompletion {
    fn new() -> Self {
        Self {
            waiters: PollSet::new(),
        }
    }
}

impl RngCompletion for HostRngCompletion {
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> driver_base::DriverResult {
        let mut task_wait = PreparedTaskWait::prepare().map_err(|_| DriverError::InvalidInput)?;
        if ready() {
            return Ok(());
        }

        let waker = task_wait.waker();
        loop {
            // Register before rechecking so a wake between check and park
            // cannot be lost; retry after spurious wakes.
            let registration = self.waiters.register(&waker).map_err(registration_error)?;
            if ready() {
                return Ok(());
            }
            task_wait.park();
            drop(registration);
        }
    }

    fn notify(&self) {
        self.waiters.wake();
    }
}

fn registration_error(error: PollRegisterError) -> DriverError {
    match error {
        PollRegisterError::NoMemory | PollRegisterError::IdExhausted => DriverError::NoMemory,
        PollRegisterError::InvalidState => DriverError::InvalidInput,
    }
}

/// Construct and publish the device for a probed transport — shared by the
/// PCI driver above and the virtio-mmio transport driver.
pub fn activate<B: driver_core::Bus>(
    dev: &mut DeviceCtx<'_, B>,
    transport: impl virtio::Transport + Send + 'static,
) -> Result<
    (
        kclass::ClassDevice<kclass::HwrngDeviceImpl>,
        driver_core::DevIrq,
    ),
    ProbeError,
> {
    let completion: Arc<dyn RngCompletion> = Arc::new(HostRngCompletion::new());
    let device = VirtIoRngDev::<driver_runtime::hal::Hal, _>::try_new(transport, completion)
        .map_err(crate::common::map_driver_error)?;

    // Completion delivery: the same single IRQ entry every driver uses. The
    // closure keeps only a weak reference — the class publication owns the
    // strong one and removal withdraws the irq before teardown.
    let weak: Weak<VirtIoRngDev<driver_runtime::hal::Hal, _>> = Arc::downgrade(&device);
    let spec = dev
        .irq(0)
        .map_err(|_| ProbeError::Fatal(RuntimeError::Invalid))?;
    let irq = dev.request_irq(spec, "virtio-rng", move || {
        let Some(device) = weak.upgrade() else {
            return IrqAction::NotHandled;
        };
        if device.handle(0).handled() {
            IrqAction::Handled
        } else {
            IrqAction::NotHandled
        }
    })?;
    device.enable_completion_irq();

    let location = dev.descriptor().location;
    let publication = publish_hwrng(
        ClassParent::detached(dev.id(), "virtio-rng", location),
        device,
    )
    .map_err(crate::common::map_driver_error)?;
    Ok((publication, irq))
}

/// virtio-mmio activation: owns the class publication; close() unpublishes
/// so nothing dereferences the transport after the mapping drops.
#[cfg(feature = "mmio")]
pub fn activate_mmio(
    dev: &mut driver_core::DeviceCtx<'_, driver_core::platform::PlatformBus>,
    transport: impl virtio::Transport + Send + 'static,
    builder: crate::mmio::ActivationBuilder,
) -> Result<crate::mmio::MmioData, ProbeError> {
    let (publication, irq) = activate(dev, transport)?;

    struct RngActivation {
        publication: kclass::ClassDevice<kclass::HwrngDeviceImpl>,
        _irq: driver_core::DevIrq,
    }

    impl crate::mmio::MmioActivation for RngActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            // Same order as the PCI removal path: withdraw the class
            // publication (full removal: callbacks, devnode, unpublish),
            // then dropping self releases the IRQ guard before the mmio
            // mapping goes away.
            kclass::unpublish_hwrng(self.publication.id());
        }
    }

    Ok(builder.build(RngActivation {
        publication,
        _irq: irq,
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
