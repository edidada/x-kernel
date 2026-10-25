// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real-host synchronous unregister and reliable-wait acceptance.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcompletion::Completion;
use kspin::{IrqSave, NoPreempt};
use subsys_hwrng::{HwrngDevice, HwrngRegistry};
use unittest::{assert, assert_eq, def_test};

use super::*;

fn wait_for(mut ready: impl FnMut() -> bool) -> bool {
    let deadline_ns = khal::time::monotonic_time_nanos().saturating_add(5_000_000_000);
    loop {
        if ready() {
            return true;
        }
        if khal::time::monotonic_time_nanos() >= deadline_ns {
            return false;
        }
        ktask::yield_now();
    }
}

#[def_test(serial)]
fn drain_preparation_rejects_atomic_context_before_predicate() {
    let drain = new_rng_drain().unwrap();
    let calls = AtomicUsize::new(0);
    let mut predicate = || {
        calls.fetch_add(1, Ordering::Relaxed);
        true
    };
    {
        let _guard = IrqSave::new();
        assert_eq!(drain.wait_until(&mut predicate), Err(KError::InvalidInput));
        assert!(matches!(new_rng_drain(), Err(KError::InvalidInput)));
    }
    {
        let _guard = NoPreempt::new();
        assert_eq!(drain.wait_until(&mut predicate), Err(KError::InvalidInput));
    }
    let result = krcu::with_read(|_| drain.wait_until(&mut predicate));
    assert_eq!(result, Err(KError::InvalidInput));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(drain.wait_until(&mut predicate), Ok(()));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let device = Arc::new(ReadingDevice {
        entered: Completion::new(),
        release: Completion::new(),
        has_finished: AtomicBool::new(false),
    });
    let mut registration = REGISTRY
        .attach_legacy("context-ut", "context-ut".into(), device, drain)
        .unwrap();
    let handle = registration.publication();
    let result = {
        let _guard = IrqSave::new();
        registration.unregister()
    };
    assert_eq!(result, Err(KError::InvalidInput));
    assert!(handle.is_available());
    assert_eq!(REGISTRY.devices().len(), 1);
    assert_eq!(registration.unregister(), Ok(()));
    assert!(!handle.is_available());
    assert!(REGISTRY.devices().is_empty());
}

#[def_test(serial)]
fn drain_preserves_early_notification_and_detaches_its_stack_waiter() {
    let drain = HostRngDrain {
        waiters: WaitQueue::new(),
    };
    drain.notify();
    assert_eq!(drain.wait_until(&mut || true), Ok(()));
    let mut checks = 0;
    let mut notified = 0;
    let result = drain.wait_until(&mut || {
        checks += 1;
        if checks == 1 {
            notified = drain.waiters.notify_all();
            false
        } else {
            true
        }
    });
    assert_eq!(result, Ok(()));
    assert_eq!(checks, 2);
    assert_eq!(notified, 1);
    assert_eq!(drain.waiters.notify_all(), 0);
}

struct ReadingDevice {
    entered: Completion,
    release: Completion,
    has_finished: AtomicBool,
}

impl HwrngDevice for ReadingDevice {
    fn read(&self, buf: &mut [u8]) -> KResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.entered.complete();
        let wait = CompletionWait::prepare().map_err(|_| KError::InvalidInput)?;
        wait.wait(&self.release);
        self.has_finished.store(true, Ordering::Release);
        buf[0] = 0x4c;
        Ok(1)
    }

    fn is_failed(&self) -> bool {
        false
    }
}

#[def_test(serial)]
fn unregister_closes_then_drains_before_notifying_with_old_handles_alive() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let device = Arc::new(ReadingDevice {
        entered: Completion::new(),
        release: Completion::new(),
        has_finished: AtomicBool::new(false),
    });
    let drain = Arc::new(HostRngDrain {
        waiters: WaitQueue::new(),
    });
    let mut registration = REGISTRY
        .attach_legacy("drain-ut", "drain-ut".into(), device.clone(), drain.clone())
        .unwrap();
    let handle = registration.publication();
    let observed = Arc::new(AtomicBool::new(false));
    let notifications = Arc::new(AtomicUsize::new(0));
    REGISTRY.subscribe_removed(Arc::new({
        let device = Arc::downgrade(&device);
        let observed = observed.clone();
        let notifications = notifications.clone();
        move |old| {
            observed.store(
                device
                    .upgrade()
                    .is_some_and(|device| device.has_finished.load(Ordering::Acquire))
                    && !old.is_available()
                    && old.is_failed() == Err(KError::BadState)
                    && REGISTRY.devices().is_empty(),
                Ordering::Release,
            );
            notifications.fetch_add(1, Ordering::Release);
        }
    }));
    let reader = ktask::spawn({
        let handle = handle.clone();
        move || {
            let mut buf = [0];
            core::assert_eq!(handle.read(&mut buf), Ok(1));
            core::assert_eq!(buf, [0x4c]);
        }
    });
    let reader_entered = wait_for(|| device.entered.try_wait());
    let reader_blocked = wait_for(|| reader.state() == ktask::TaskState::Blocked);
    let completed = Arc::new(AtomicBool::new(false));
    let succeeded = Arc::new(AtomicBool::new(false));
    let unregistration = ktask::spawn({
        let completed = completed.clone();
        let succeeded = succeeded.clone();
        let notifications = notifications.clone();
        move || {
            let first = registration.unregister();
            let second = registration.unregister();
            succeeded.store(
                first == Ok(()) && second == Ok(()) && notifications.load(Ordering::Acquire) == 1,
                Ordering::Release,
            );
            completed.store(true, Ordering::Release);
        }
    });
    let admission_closed = wait_for(|| !handle.is_available());
    let unregister_reached_wait = wait_for(|| {
        unregistration.state() == ktask::TaskState::Blocked || completed.load(Ordering::Acquire)
    });
    let blocked = unregistration.state() == ktask::TaskState::Blocked;
    let completed_early = completed.load(Ordering::Acquire);
    let notifications_before_release = notifications.load(Ordering::Acquire);
    let new_read = handle.read(&mut []);
    let new_query = handle.is_failed();
    let registry_is_empty = REGISTRY.devices().is_empty();
    device.release.complete_all();
    let reader_exit =
        wait_for(|| reader.state() == ktask::TaskState::Exited).then(|| reader.join());
    let unregister_exit = wait_for(|| unregistration.state() == ktask::TaskState::Exited)
        .then(|| unregistration.join());
    assert!(reader_entered && reader_blocked && admission_closed && unregister_reached_wait);
    assert!(blocked && !completed_early && registry_is_empty);
    assert_eq!(notifications_before_release, 0);
    assert_eq!(new_read, Err(KError::BadState));
    assert_eq!(new_query, Err(KError::BadState));
    assert_eq!(reader_exit, Some(0));
    assert_eq!(unregister_exit, Some(0));
    assert!(succeeded.load(Ordering::Acquire));
    assert!(observed.load(Ordering::Acquire));
    assert_eq!(notifications.load(Ordering::Acquire), 1);
    assert_eq!(drain.waiters.notify_all(), 0);
    assert!(!handle.is_available());
}
