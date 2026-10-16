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

use driver_base::{
    BoundDevice, DeviceCtx, DriverDesc, DriverOps, IrqAction, ProbeError, RuntimeError,
};
use kerrno::{KError, KResult};
use kpoll::{PollRegisterError, PollSet};
use ktask::future::PreparedTaskWait;
use pci::{PciBus, PciMatch};
use subsys_hwrng::{class::HWRNG, completion::RngCompletion};
use virtio_devices::{VirtIoRngDev, VirtioDeviceKind};

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
    /// The Data-owned publication: dropping unpublishes (first field —
    /// before the IRQ guard below releases).
    _publication: driver_base::ClassWithdraw,
    _irq: driver_base::DevIrq,
}

/// The driver ops.
pub struct VirtioRngDriver;

impl DriverOps<PciBus> for VirtioRngDriver {
    type Data = RngData;

    fn probe(&self, dev: &mut DeviceCtx<'_, PciBus>) -> Result<RngData, ProbeError> {
        let (kind, transport, _irq) = crate::common::pci_transport(dev)?;
        if !matches!(kind, VirtioDeviceKind::Hwrng) {
            return Err(ProbeError::Fatal(RuntimeError::Unsupported));
        }

        // The publication rides RngData (withdrawal guard): the Data-owner
        // contract — no composition-root class drain at remove.
        let (publication, irq) = activate(dev, transport, None)?;
        Ok(RngData {
            _publication: subsys_hwrng::class::HWRNG.withdraw(&publication),
            _irq: irq,
        })
    }

    fn remove(&self, _dev: &mut BoundDevice<'_, PciBus, RngData>) {
        // The Data-owned publication guard unpublishes at Data drop.
    }
}

/// The host completion capability (main's `rng_completion` host provider):
/// task-context wait on a `PollSet`, IRQ-safe wake.
///
/// # Execution context
///
/// [`RngCompletion::wait_until`] **must run in task context**. It calls
/// [`PreparedTaskWait::prepare`], which parks the *current task*; from hard-IRQ
/// or softirq context there is no task to park, and `prepare()` fails — the
/// error surfaces as [`KError::InvalidInput`], not as a hang, but the
/// entropy read fails. [`RngCompletion::notify`] is the IRQ-side half and is
/// safe from interrupt context: it only wakes.
///
/// Today the sole caller chain is
/// `util/entropy` → `hwrng` class → `VirtIoRngDev::read` → `wait_until`, all of
/// which run on the entropy collector's task. **If you add a second caller,
/// check its context first.**
///
/// # Registered exception E-04
///
/// This is the one place under `drv/` that names `ktask` and `kpoll` directly.
/// `subsystems/subsys_block::dispatch::wait` already wraps the identical primitive
/// (`PreparedTaskWait` + a poll registration, same register-before-recheck
/// ordering) as a subsystem *service* — that is where this belongs, and the
/// hwrng side should get the same `subsystems/subsys_hwrng::dispatch` module. It has
/// not, because hwrng has exactly one consumer and the project's own rule is
/// not to build machinery ahead of a second one
/// (`docs/driver-framework-review.md` §8.3 class B). **Fold this into a service
/// when a second hwrng driver appears.** See
/// `docs/ai/driver-subsystem-boundaries.md`, exception E-04.
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
    /// Block until `ready()` holds. **Task context only** — see the
    /// execution-context note on [`HostRngCompletion`].
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> KResult {
        let mut task_wait = PreparedTaskWait::prepare().map_err(|_| KError::InvalidInput)?;
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

fn registration_error(error: PollRegisterError) -> KError {
    match error {
        PollRegisterError::NoMemory | PollRegisterError::IdExhausted => KError::NoMemory,
        PollRegisterError::InvalidState => KError::InvalidInput,
    }
}

/// Construct and publish the device for a probed transport — shared by the
/// PCI driver above and the virtio-mmio transport driver.
pub fn activate<B: driver_base::Bus>(
    dev: &mut DeviceCtx<'_, B>,
    transport: impl virtio_devices::Transport + Send + 'static,
    mmio_regs: Option<driver_base::IoMap>,
) -> Result<
    (
        driver_base::ClassDevice<subsys_hwrng::HwrngDeviceImpl>,
        driver_base::DevIrq,
    ),
    ProbeError,
> {
    let completion: Arc<dyn RngCompletion> = Arc::new(HostRngCompletion::new());
    let device =
        VirtIoRngDev::<driver_host::virtio_hal::Hal, _>::try_new(transport, completion, mmio_regs)
            .map_err(crate::common::map_driver_error)?;

    // Completion delivery: the same single IRQ entry every driver uses. The
    // closure keeps only a weak reference — the class publication owns the
    // strong one and removal withdraws the irq before teardown.
    let weak: Weak<VirtIoRngDev<driver_host::virtio_hal::Hal, _>> = Arc::downgrade(&device);
    let spec = dev
        .irq(0)
        .map_err(|_| ProbeError::Fatal(RuntimeError::Invalid))?;
    let irq = dev.request_irq(spec, "virtio-rng", move || {
        let Some(device) = weak.upgrade() else {
            return IrqAction::NotHandled;
        };
        if device.ack_irq(0) {
            IrqAction::Handled
        } else {
            IrqAction::NotHandled
        }
    })?;
    device.enable_completion_irq();

    let publication = HWRNG
        .publish(dev.class_parent(), device, "virtio-rng")
        .map_err(crate::common::map_driver_error)?;
    Ok((publication, irq))
}

/// virtio-mmio activation: owns the class publication; close() unpublishes
/// so nothing dereferences the transport after the mapping drops.
#[cfg(feature = "mmio")]
pub fn activate_mmio(
    dev: &mut driver_base::DeviceCtx<'_, driver_base::platform::PlatformBus>,
    transport: impl virtio_devices::Transport + Send + 'static,
    mut builder: crate::mmio::ActivationBuilder,
) -> Result<crate::mmio::MmioData, ProbeError> {
    // The MMIO register mapping follows the device object (Linux BAR
    // model): the last ClassDevice clone releases it.
    let mmio_regs = builder.take_regs();
    let (publication, irq) = activate(dev, transport, mmio_regs)?;

    struct RngActivation {
        // Field order = teardown order (same as the PCI Data): withdraw the
        // class publication first (full removal: callbacks, devnode,
        // unpublish) — consumers detach while the device is still live —
        // then the IRQ guard releases, and the mmio mapping goes with the
        // device object (the last ClassDevice clone).
        _withdraw: driver_base::ClassWithdraw,
        _irq: driver_base::DevIrq,
    }

    impl crate::mmio::MmioActivation for RngActivation {
        fn close(self: alloc::boxed::Box<Self>) {
            // Field order does the sequencing.
        }
    }

    Ok(builder.build(RngActivation {
        _withdraw: subsys_hwrng::class::HWRNG.withdraw(&publication),
        _irq: irq,
    }))
}

/// `module_pci_driver()` equivalent.
pub fn register() {
    driver_base::global_domain().register_driver(DriverDesc::new(
        "virtio-rng",
        IDS,
        VirtioRngDriver,
    ));
}
