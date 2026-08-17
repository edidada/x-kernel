// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ-deferred completion plumbing shared by the VirtIO block and RNG
//! adapters.
//!
//! This module implements the Linux-style split between the top and bottom
//! half of a device interrupt:
//!
//! 1. **hardirq handler**: acknowledges the device interrupt status, marks the
//!    per-device entry pending, and raises the device-class softirq vector.
//!    It never processes completions or touches waiter state beyond the
//!    pending flag.
//! 2. **softirq action**: runs from the IRQ-tail or `ksoftirqd` drain path,
//!    consumes the pending flags, and wakes the sleeping requester through
//!    [`kpoll::Completion`], whose wake side is IRQ-safe.
//! 3. **requester task**: submits the device request, then sleeps until the
//!    softirq wake and rechecks its own device-side completion predicate
//!    (the used-ring token) after every wake, because the completion is only
//!    a wake source, not a result carrier.
//!
//! When the current context cannot sleep (early boot, IRQ-context caller), the
//! requester degrades to polling the device predicate with local IRQs
//! enabled; the registered hardirq handler keeps acknowledging the device ISR,
//! so a shared level-triggered line cannot livelock the CPU.

use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use device_res::{Irq, IrqEvent, IrqResource, IrqTrigger};
use kirq::softirq::SoftirqVec;
use kpoll::Completion;
use kspin::SpinNoIrq;

/// Per-device completion state shared between the hardirq handler, the
/// deferred softirq action, and the requester task.
pub(crate) struct IrqCompletionEntry {
    irq: usize,
    /// Set by the hardirq handler after it acknowledged a real device
    /// interrupt; cleared by the softirq action when it wakes waiters.
    is_pending: AtomicBool,
    /// Wake source for the sleeping requester. Tokens may be consumed out of
    /// order with completions, so waiters must always recheck their real
    /// device-side predicate after waking.
    completion: Completion,
}

impl IrqCompletionEntry {
    /// Creates an entry listening on `irq`.
    pub(crate) fn new(irq: usize) -> Self {
        Self {
            irq,
            is_pending: AtomicBool::new(false),
            completion: Completion::new(),
        }
    }

    /// The OS-visible IRQ number this entry listens on.
    pub(crate) fn irq(&self) -> usize {
        self.irq
    }

    /// The wake source waited on by the requester task.
    pub(crate) fn completion(&self) -> &Completion {
        &self.completion
    }

    /// Consumes one pending wake token, if any.
    ///
    /// Useful for callers that must poll the wake source without a sleepable
    /// current task.
    pub(crate) fn try_wait(&self) -> bool {
        self.completion.try_wait()
    }

    /// Waits for one softirq wake in a sleepable task context.
    ///
    /// Returns `false` when the current context cannot sleep (no current
    /// task); callers must then fall back to polling their device-side
    /// completion predicate. Returning `true` only means a wake was observed
    /// or a stale token was consumed — the caller must recheck its real
    /// predicate.
    pub(crate) fn wait_wake(&self) -> bool {
        kirq::IrqSyncWaitIf::wait_for_completion(&self.completion).is_ok()
    }

    fn mark_pending(&self) {
        self.is_pending.store(true, Ordering::Release);
    }

    fn take_pending(&self) -> bool {
        self.is_pending.swap(false, Ordering::AcqRel)
    }
}

/// A device whose hardirq acknowledgement defers waiter wakeups through a
/// softirq vector.
pub(crate) trait IrqCompletionDevice: Send + Sync {
    /// Acknowledges the device interrupt status and reports whether the
    /// device actually raised this IRQ.
    fn ack_interrupt(&self) -> bool;

    /// The shared completion entry for this device.
    fn entry(&self) -> &Arc<IrqCompletionEntry>;

    /// The OS-visible IRQ number this device is registered on.
    fn irq(&self) -> usize {
        self.entry().irq()
    }
}

/// Registry binding one softirq vector to the VirtIO devices using it.
///
/// Each device kind (block, RNG) owns one hub as a crate-level static and
/// passes its own softirq action `fn()` to [`IrqCompletionHub::register`],
/// because softirq actions are plain function pointers.
pub(crate) struct IrqCompletionHub {
    vec: SoftirqVec,
    devices: SpinNoIrq<Vec<Arc<dyn IrqCompletionDevice>>>,
    registered_irqs: SpinNoIrq<BTreeSet<usize>>,
    /// Keeps the `device_res::Irq` guards alive for the kernel lifetime;
    /// dropping one would release the hardirq handler.
    guards: SpinNoIrq<Vec<Irq>>,
    is_softirq_open: SpinNoIrq<bool>,
}

impl IrqCompletionHub {
    /// Creates a hub serving the given softirq vector.
    pub(crate) const fn new(vec: SoftirqVec) -> Self {
        Self {
            vec,
            devices: SpinNoIrq::new(Vec::new()),
            registered_irqs: SpinNoIrq::new(BTreeSet::new()),
            guards: SpinNoIrq::new(Vec::new()),
            is_softirq_open: SpinNoIrq::new(false),
        }
    }

    /// Registers `device` on the hub and requests its IRQ line.
    ///
    /// `softirq_action` must forward to [`IrqCompletionHub::run_softirq`] on
    /// this hub; it is installed exactly once as the vector's action.
    ///
    /// Returns `None` when the IRQ line cannot be requested; the hub is left
    /// unchanged in that case so the caller keeps its synchronous polling
    /// fallback.
    pub(crate) fn register(
        &'static self,
        softirq_action: fn(),
        device: Arc<dyn IrqCompletionDevice>,
    ) -> Option<Arc<IrqCompletionEntry>> {
        let entry = device.entry().clone();
        let irq = device.irq();

        {
            let mut installed = self.is_softirq_open.lock();
            if !*installed {
                if kirq::softirq::open_softirq(self.vec, softirq_action) {
                    *installed = true;
                } else {
                    log::warn!(
                        "virtio: softirq vector {} already registered; completions stay pending",
                        self.vec.as_usize()
                    );
                }
            }
        }

        self.devices.lock().push(device.clone());

        if self.registered_irqs.lock().insert(irq) {
            let resource = IrqResource::new(irq, IrqTrigger::Unknown(0));
            match Irq::request(resource, Arc::new(HubIrqHandler(self))) {
                Ok(guard) => self.guards.lock().push(guard),
                Err(err) => {
                    warn_and_rollback(self, &device, err);
                    return None;
                }
            }
        }

        Some(entry)
    }

    /// Removes a previously registered device and releases its IRQ line when
    /// no other device shares it.
    pub(crate) fn unregister(&self, device: &Arc<dyn IrqCompletionDevice>) {
        self.devices.lock().retain(|dev| !Arc::ptr_eq(dev, device));

        let irq_still_used = self
            .devices
            .lock()
            .iter()
            .any(|dev| dev.irq() == device.irq());
        if !irq_still_used && self.registered_irqs.lock().remove(&device.irq()) {
            let guard = {
                let mut guards = self.guards.lock();
                guards
                    .iter()
                    .position(|guard| guard.number() == device.irq())
                    .map(|index| guards.swap_remove(index))
            };
            // Dropping the guard releases the hardirq handler; any requester
            // still parked on the entry rechecks its predicate on its own.
            drop(guard);
        }
    }

    /// hardirq entry: acknowledge matching devices and defer wakeups.
    fn handle_irq(&self, irq: usize) -> IrqEvent {
        let mut handled = false;
        {
            let devices = self.devices.lock();
            for device in devices.iter() {
                if device.irq() != irq {
                    continue;
                }
                if device.ack_interrupt() {
                    device.entry().mark_pending();
                    handled = true;
                }
            }
        }
        if handled {
            kirq::softirq::raise_softirq(self.vec);
            IrqEvent::HANDLED
        } else {
            IrqEvent::NOT_HANDLED
        }
    }

    /// softirq action: wake the requesters of every device the hardirq marked
    /// pending.
    pub(crate) fn run_softirq(&self) {
        let devices = self.devices.lock();
        for device in devices.iter() {
            let entry = device.entry();
            if entry.take_pending() {
                entry.completion().complete();
            }
        }
    }

    /// Number of registered devices (unit-test introspection only).
    #[cfg(unittest)]
    pub(crate) fn device_count(&self) -> usize {
        self.devices.lock().len()
    }
}

/// `device_res` handler bound to one hub. It exists because handler objects
/// must own a `'static` reference to their hub.
struct HubIrqHandler(&'static IrqCompletionHub);

impl device_res::IrqHandler for HubIrqHandler {
    fn handle(&self, irq: usize) -> IrqEvent {
        self.0.handle_irq(irq)
    }
}

fn warn_and_rollback(
    hub: &IrqCompletionHub,
    device: &Arc<dyn IrqCompletionDevice>,
    err: device_res::ResError,
) {
    log::warn!(
        "virtio: failed to register IRQ {} handler: {:?}; device keeps polling mode",
        device.irq(),
        err
    );
    hub.registered_irqs.lock().remove(&device.irq());
    hub.devices.lock().retain(|dev| !Arc::ptr_eq(dev, device));
}

/// Installs an IRQ provider whose line requests always fail, so hub
/// registration deterministically rolls back in every test environment
/// (with or without a real host provider already installed).
#[cfg(unittest)]
pub(crate) mod test_support {
    use alloc::sync::Arc;

    use device_res::{IrqHandler, IrqHandlerToken, IrqOp, IrqResource};

    struct FailingIrqOp;

    impl IrqOp for FailingIrqOp {
        fn request_irq(
            &self,
            _irq: IrqResource,
            _handler: Arc<dyn IrqHandler>,
        ) -> device_res::ResResult<IrqHandlerToken> {
            Err(device_res::ResError::Busy)
        }

        fn release_irq(&self, _irq: IrqResource, _token: IrqHandlerToken) {}

        fn set_irq_enabled(&self, _irq: IrqResource, _enabled: bool) {}
    }

    static FAILING_IRQ_OP: FailingIrqOp = FailingIrqOp;

    pub(crate) struct ScopedFailingIrqProvider {
        saved: Option<&'static dyn IrqOp>,
    }

    impl ScopedFailingIrqProvider {
        pub(crate) fn new() -> Self {
            let saved = device_res::try_irq_provider();
            device_res::set_irq_provider(&FAILING_IRQ_OP);
            Self { saved }
        }
    }

    impl Drop for ScopedFailingIrqProvider {
        fn drop(&mut self) {
            if let Some(provider) = self.saved {
                device_res::set_irq_provider(provider);
            }
        }
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::irq_completion::test_support::ScopedFailingIrqProvider;

    static TEST_HUB: IrqCompletionHub = IrqCompletionHub::new(SoftirqVec::High);
    fn test_hub_softirq() {
        TEST_HUB.run_softirq();
    }

    struct FakeDevice {
        entry: Arc<IrqCompletionEntry>,
        ack_raised: AtomicBool,
    }

    impl FakeDevice {
        fn new(irq: usize) -> Arc<Self> {
            Arc::new(Self {
                entry: Arc::new(IrqCompletionEntry::new(irq)),
                ack_raised: AtomicBool::new(false),
            })
        }
    }

    // SAFETY: all shared state is either atomic or guarded by the hub's
    // SpinNoIrq registries.
    unsafe impl Send for FakeDevice {}
    // SAFETY: same as above; no interior mutability escapes the atomics.
    unsafe impl Sync for FakeDevice {}

    impl IrqCompletionDevice for FakeDevice {
        fn ack_interrupt(&self) -> bool {
            self.ack_raised.load(Ordering::Acquire)
        }

        fn entry(&self) -> &Arc<IrqCompletionEntry> {
            &self.entry
        }
    }

    fn reset_test_hub() {
        TEST_HUB.devices.lock().clear();
        TEST_HUB.registered_irqs.lock().clear();
        TEST_HUB.guards.lock().clear();
        // Drop guards first so handler release ordering cannot matter here.
        kirq::softirq::clear_softirq_pending_for_tests(SoftirqVec::High);
    }

    #[def_test(serial)]
    fn test_hub_softirq_wakes_pending_devices() {
        reset_test_hub();

        let device = FakeDevice::new(90);
        let entry = device.entry().clone();
        device.ack_raised.store(true, Ordering::Release);
        TEST_HUB.devices.lock().push(device);

        let event = TEST_HUB.handle_irq(90);
        assert!(event.handled());
        assert!(entry.is_pending.load(Ordering::Acquire));

        TEST_HUB.run_softirq();
        assert!(!entry.is_pending.load(Ordering::Acquire));
        assert!(entry.completion().try_wait());

        reset_test_hub();
    }

    #[def_test(serial)]
    fn test_hub_ignores_unraised_or_foreign_irq() {
        reset_test_hub();

        let raised = FakeDevice::new(91);
        let silent = FakeDevice::new(92);
        silent.ack_raised.store(true, Ordering::Release);
        TEST_HUB
            .devices
            .lock()
            .push(raised.clone() as Arc<dyn IrqCompletionDevice>);
        TEST_HUB.devices.lock().push(silent.clone());

        assert!(!TEST_HUB.handle_irq(91).handled());
        assert!(!TEST_HUB.handle_irq(93).handled());
        assert!(!raised.entry().is_pending.load(Ordering::Acquire));
        assert!(!silent.entry().is_pending.load(Ordering::Acquire));

        reset_test_hub();
    }

    #[def_test(serial)]
    fn test_run_softirq_without_pending_is_noop() {
        reset_test_hub();

        let device = FakeDevice::new(94);
        let entry = device.entry().clone();
        device.ack_raised.store(true, Ordering::Release);
        TEST_HUB.devices.lock().push(device);

        TEST_HUB.run_softirq();
        assert!(!entry.completion().try_wait());

        reset_test_hub();
    }

    #[def_test(serial)]
    fn test_register_rolls_back_when_line_request_fails() {
        reset_test_hub();
        let _failing_provider = ScopedFailingIrqProvider::new();

        let device: Arc<dyn IrqCompletionDevice> = FakeDevice::new(95);
        assert!(
            TEST_HUB
                .register(test_hub_softirq, device.clone())
                .is_none()
        );
        assert_eq!(TEST_HUB.device_count(), 0);

        TEST_HUB.unregister(&device);
        assert_eq!(TEST_HUB.device_count(), 0);

        reset_test_hub();
    }
}
