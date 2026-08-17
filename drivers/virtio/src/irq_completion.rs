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
//!    consumes the pending flags, and broadcasts a wake on the entry's
//!    [`PollSet`].
//! 3. **requester task**: submits the device request, then sleeps until a
//!    broadcast wake and rechecks its own device-side completion predicate
//!    (the used-ring token) after every wake.
//!
//! The wake side is deliberately a broadcast [`PollSet`] rather than a
//! token-counting completion: one interrupt may cover several completed
//! requests (the virtio ISR is per-device and the pending flag collapses
//! multiple hardirqs into one softirq run), so wake accounting must not be
//! coupled to interrupt counts. Waiters are always responsible for rechecking
//! their real predicate; a wake never carries a result.
//!
//! When the current context cannot sleep (early boot, IRQ-context caller),
//! the wait helpers return `false` and the requester degrades to polling the
//! device predicate with local IRQs enabled; the registered hardirq handler
//! keeps acknowledging the device ISR, so a shared level-triggered line
//! cannot livelock the CPU.

use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};

use device_res::{Irq, IrqEvent, IrqResource, IrqTrigger};
use kirq::softirq::SoftirqVec;
use kpoll::PollSet;
use kspin::SpinNoIrq;

/// Per-device completion state shared between the hardirq handler, the
/// deferred softirq action, and the requester task.
pub(crate) struct IrqCompletionEntry {
    irq: usize,
    /// Set by the hardirq handler after it acknowledged a real device
    /// interrupt; cleared by the softirq action when it broadcasts the wake.
    is_pending: AtomicBool,
    /// Broadcast wake source for sleeping requesters. Wakes carry no result;
    /// every waiter rechecks its own device-side predicate after waking.
    wake: PollSet,
}

impl IrqCompletionEntry {
    /// Creates an entry listening on `irq`.
    pub(crate) fn new(irq: usize) -> Self {
        Self {
            irq,
            is_pending: AtomicBool::new(false),
            wake: PollSet::new(),
        }
    }

    /// The OS-visible IRQ number this entry listens on.
    pub(crate) fn irq(&self) -> usize {
        self.irq
    }

    /// The broadcast wake source for this entry.
    pub(crate) fn wake_source(&self) -> &PollSet {
        &self.wake
    }

    /// Sleeps in the current task until `predicate` holds, waking on every
    /// broadcast from the softirq action.
    ///
    /// Returns `false` when the current context cannot sleep (no current
    /// task); callers must then poll the predicate themselves with local
    /// IRQs enabled. Returning `true` only means the predicate held at some
    /// poll — callers that re-check naturally after this call are already
    /// correct.
    pub(crate) fn wait_until(&self, predicate: impl FnMut() -> bool) -> bool {
        wait_poll_set(&self.wake, predicate)
    }

    /// Like [`Self::wait_until`], bounded by `deadline`.
    ///
    /// Returns `false` when the current context cannot sleep; callers must
    /// then poll the predicate themselves until the deadline.
    pub(crate) fn wait_until_deadline(
        &self,
        deadline: ktime_types::MonotonicInstant,
        predicate: impl FnMut() -> bool,
    ) -> bool {
        wait_poll_set_deadline(&self.wake, deadline, predicate)
    }

    fn mark_pending(&self) {
        self.is_pending.store(true, Ordering::Release);
    }

    fn take_pending(&self) -> bool {
        self.is_pending.swap(false, Ordering::AcqRel)
    }
}

/// Sleeps in the current task on a broadcast wake source until `predicate`
/// holds.
///
/// This is the kernel wait_queue pattern: register, recheck, sleep; every
/// broadcast wakes all waiters and each waiter rechecks its own predicate.
/// Returns `false` when no sleepable current task exists.
pub(crate) fn wait_poll_set(source: &PollSet, mut predicate: impl FnMut() -> bool) -> bool {
    if ktask::current_may_uninit().is_none() {
        return false;
    }

    let mut registrations = kpoll::PollRegistrations::new();
    ktask::future::block_on(core::future::poll_fn(|cx| {
        if predicate() {
            return Poll::Ready(());
        }
        // The register/recheck pair closes the wake-vs-register race: a wake
        // that fires between the first check and the registration either
        // detaches the freshly registered waker (waking us) or is covered by
        // the second check.
        let mut context = registrations.context(cx);
        let _ = context.register(source);
        drop(context);
        if predicate() {
            return Poll::Ready(());
        }
        Poll::Pending
    }));
    true
}

/// Deadline-bounded variant of [`wait_poll_set`].
pub(crate) fn wait_poll_set_deadline(
    source: &PollSet,
    deadline: ktime_types::MonotonicInstant,
    mut predicate: impl FnMut() -> bool,
) -> bool {
    if ktask::current_may_uninit().is_none() {
        return false;
    }

    let mut registrations = kpoll::PollRegistrations::new();
    let _ = ktask::future::block_on(ktask::future::timeout_at(
        Some(deadline),
        core::future::poll_fn(|cx| {
            if predicate() {
                return Poll::Ready(());
            }
            let mut context = registrations.context(cx);
            let _ = context.register(source);
            drop(context);
            if predicate() {
                return Poll::Ready(());
            }
            Poll::Pending
        }),
    ));
    true
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
    /// Returns `None` when the softirq vector cannot be claimed or the IRQ
    /// line cannot be requested; the hub is left unchanged in that case so
    /// the caller keeps its synchronous polling fallback. A vector owned by
    /// another subsystem would never run this hub's action, so its waiters
    /// could never be woken.
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
                        "virtio: softirq vector {} is owned by another subsystem; device keeps \
                         polling mode",
                        self.vec.as_usize()
                    );
                    return None;
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

    /// softirq action: broadcast a wake for every device the hardirq marked
    /// pending.
    ///
    /// One broadcast serves any number of waiters: an interrupt may cover
    /// several completed requests, and each waiter rechecks its own
    /// device-side predicate after waking.
    pub(crate) fn run_softirq(&self) {
        let devices = self.devices.lock();
        for device in devices.iter() {
            let entry = device.entry();
            if entry.take_pending() {
                entry.wake_source().wake();
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
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
    fn test_hub_softirq_wakes_registered_waiters() {
        reset_test_hub();

        let device = FakeDevice::new(90);
        let entry = device.entry().clone();
        device.ack_raised.store(true, Ordering::Release);
        TEST_HUB.devices.lock().push(device);

        // Two waiters registered on the broadcast source model two in-flight
        // requesters; a single softirq run must wake both.
        let first_wakes = Arc::new(AtomicUsize::new(0));
        let second_wakes = Arc::new(AtomicUsize::new(0));
        let first = register_counting_waker(entry.wake_source(), &first_wakes);
        let second = register_counting_waker(entry.wake_source(), &second_wakes);

        let event = TEST_HUB.handle_irq(90);
        assert!(event.handled());
        assert!(entry_is_pending(&entry));

        TEST_HUB.run_softirq();
        assert!(!entry_is_pending(&entry));
        assert_eq!(first_wakes.load(Ordering::SeqCst), 1);
        assert_eq!(second_wakes.load(Ordering::SeqCst), 1);

        drop(first);
        drop(second);
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
        assert!(!entry_is_pending(raised.entry()));
        assert!(!entry_is_pending(silent.entry()));

        reset_test_hub();
    }

    #[def_test(serial)]
    fn test_run_softirq_without_pending_does_not_wake() {
        reset_test_hub();

        let device = FakeDevice::new(94);
        let entry = device.entry().clone();
        device.ack_raised.store(true, Ordering::Release);
        TEST_HUB.devices.lock().push(device);
        let wakes = Arc::new(AtomicUsize::new(0));
        let registration = register_counting_waker(entry.wake_source(), &wakes);

        TEST_HUB.run_softirq();
        assert_eq!(wakes.load(Ordering::SeqCst), 0);

        drop(registration);
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

    #[def_test(serial)]
    fn test_register_rolls_back_when_vector_is_owned_elsewhere() {
        reset_test_hub();
        let _failing_provider = ScopedFailingIrqProvider::new();

        // Occupy the vector with a foreign action; the hub must refuse to
        // register and leave the device unregistered so the caller keeps its
        // polling fallback.
        fn foreign_action() {}
        let _foreign = kirq::softirq::test_support::ScopedSoftirqAction::install(
            SoftirqVec::High,
            foreign_action,
        );
        let device: Arc<dyn IrqCompletionDevice> = FakeDevice::new(96);
        assert!(
            TEST_HUB
                .register(test_hub_softirq, device.clone())
                .is_none()
        );
        assert_eq!(TEST_HUB.device_count(), 0);

        TEST_HUB.unregister(&device);
        reset_test_hub();
    }

    #[def_test]
    fn test_wait_until_returns_immediately_for_true_predicate() {
        let entry = IrqCompletionEntry::new(97);
        // Kernel unit tests run in a task context, so the sleepable path is
        // exercised; a true predicate must resolve without any wake.
        assert!(entry.wait_until(|| true));
    }

    /// Registers a manually-counted waker on `source`.
    fn register_counting_waker(
        source: &PollSet,
        counter: &Arc<AtomicUsize>,
    ) -> kpoll::PollRegistration {
        struct CountingWaker {
            counter: Arc<AtomicUsize>,
        }
        impl alloc::task::Wake for CountingWaker {
            fn wake(self: Arc<Self>) {
                self.counter.fetch_add(1, Ordering::SeqCst);
            }
        }
        let waker = core::task::Waker::from(Arc::new(CountingWaker {
            counter: counter.clone(),
        }));
        source
            .register(&waker)
            .expect("test waker registration must succeed")
    }

    fn entry_is_pending(entry: &IrqCompletionEntry) -> bool {
        entry.is_pending.load(Ordering::Acquire)
    }
}
