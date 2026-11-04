// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Opt-in real pending-read teardown with a host-controlled withholding rng-egd.
//!
//! The host must withhold every byte from boot and corroborate EGD requests with
//! zero replies. This sole guest reader makes the request correlation unique;
//! socket and console buffering do not establish cross-FD chronology. Guest task
//! state alone is not proof of submitted DMA. CLOSED is emitted only after
//! cleanup, restoration and all checks pass; no host acknowledgement is needed.
//! Diagnostic deadlines only fail the test; they never establish correctness.
//! A hung unbind/reset needs the outer QEMU timeout. No production hooks, forced
//! task reclamation or post-rebind entropy requests are used.
//! Dropping the observer's task reference does not cancel a live task: scheduler
//! and completion-waiter references retain it, and its closure owns all inputs.
//! Failed cleanup skips rebinding and relies on the outer guest timeout.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use driver_model::{DeviceRef, DeviceState, RuntimeError, global_domain};
use kspin::SpinNoPreempt;
use subsys_hwrng::{HWRNG, HwrngHandle};
use unittest::{assert, assert_eq, def_test};

const DRIVER_NAME: &str = "virtio-rng-mmio-model";
const WATCHDOG_NS: u64 = 5_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadResult {
    result: kerrno::KResult<usize>,
    buffer: [u8; 16],
}

struct ReaderObservation {
    started: AtomicBool,
    finished: AtomicBool,
    result: SpinNoPreempt<Option<ReadResult>>,
}

fn wait_for(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = khal::time::monotonic_time_nanos().saturating_add(WATCHDOG_NS);
    loop {
        if predicate() {
            return true;
        }
        if khal::time::monotonic_time_nanos() >= deadline {
            return false;
        }
        ktask::yield_now();
    }
}

fn publications() -> Vec<HwrngHandle> {
    HWRNG
        .devices()
        .into_iter()
        .filter(|handle| handle.driver_name() == Some(DRIVER_NAME))
        .collect()
}

fn is_restored(device: &DeviceRef) -> bool {
    global_domain().snapshot(device).is_some_and(|snapshot| {
        snapshot.state == DeviceState::Bound
            && snapshot.driver == Some(DRIVER_NAME)
            && snapshot.device.same_device(device)
    })
}

struct PendingReader {
    device: DeviceRef,
    task: Option<ktask::KtaskRef>,
    observation: Arc<ReaderObservation>,
    needs_restore: bool,
    can_restore: bool,
}

impl PendingReader {
    fn start(device: DeviceRef, handle: HwrngHandle) -> Self {
        let observation = Arc::new(ReaderObservation {
            started: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            result: SpinNoPreempt::new(None),
        });
        let reader_observation = observation.clone();
        let task = ktask::spawn(move || {
            let mut buffer = [0xa5u8; 16];
            reader_observation.started.store(true, Ordering::Release);
            let result = handle.read(&mut buffer);
            *reader_observation.result.lock() = Some(ReadResult { result, buffer });
            reader_observation.finished.store(true, Ordering::Release);
        });
        Self {
            device,
            task: Some(task),
            observation,
            needs_restore: true,
            can_restore: false,
        }
    }

    fn observe_blocked(&self) -> bool {
        let Some(task) = &self.task else {
            return false;
        };
        let observed = wait_for(|| {
            self.observation.finished.load(Ordering::Acquire)
                || (self.observation.started.load(Ordering::Acquire)
                    && task.state() == ktask::TaskState::Blocked)
        });
        observed
            && self.observation.started.load(Ordering::Acquire)
            && task.state() == ktask::TaskState::Blocked
            && !self.observation.finished.load(Ordering::Acquire)
    }

    fn stop(&mut self) -> (Result<(), RuntimeError>, Option<i32>) {
        let unbind = global_domain().unbind_device(&self.device);
        let exit = self.task.take().and_then(|task| {
            if wait_for(|| task.state() == ktask::TaskState::Exited) {
                Some(task.join())
            } else {
                log::error!("MODEL_MMIO_PENDING: reader exit watchdog failed; skipping rebind");
                None
            }
        });
        self.can_restore = unbind.is_ok() && exit == Some(0);
        (unbind, exit)
    }

    fn restore(&mut self) -> Result<(), RuntimeError> {
        if !self.can_restore {
            return Err(RuntimeError::BadState);
        }
        global_domain().bind_device(&self.device)?;
        if !is_restored(&self.device) {
            return Err(RuntimeError::BadState);
        }
        self.needs_restore = false;
        Ok(())
    }
}

impl Drop for PendingReader {
    fn drop(&mut self) {
        if self.task.is_some() {
            let (unbind, exit) = self.stop();
            if unbind.is_err() || exit != Some(0) {
                log::error!("MODEL_MMIO_PENDING: fallback cleanup failed: {unbind:?}, {exit:?}");
            }
        }
        if self.needs_restore
            && self.can_restore
            && let Err(error) = self.restore()
        {
            log::error!("MODEL_MMIO_PENDING: restoration failed: {error:?}; registration retained");
        }
    }
}

#[def_test(serial)]
fn real_model_mmio_rng_pending_unbind_releases_reader() {
    if option_env!("XK_TEST_MODEL_MMIO_PENDING") != Some("1") {
        return unittest::TestResult::Ignored;
    }
    assert!(cfg!(target_arch = "aarch64"));
    assert!(kbuild_config::KFEAT_VIRTIO_BUS_MMIO);
    assert!(!kbuild_config::KFEAT_ENTROPY_VIRTIO_RNG);
    assert!(option_env!("XK_TEST_MODEL_MMIO_LIFECYCLE") != Some("1"));
    assert!(ktask::is_sleepable_context());
    let domain = global_domain();
    let mut devices: Vec<_> = domain
        .devices()
        .into_iter()
        .filter(|snapshot| snapshot.driver == Some(DRIVER_NAME))
        .collect();
    let mut initial = publications();
    assert_eq!(devices.len(), 1);
    assert_eq!(initial.len(), 1);
    let snapshot = devices.pop().unwrap();
    assert_eq!(snapshot.state, DeviceState::Bound);
    let device = snapshot.device;
    let old = initial.pop().unwrap();
    assert!(old.is_available());
    let mut reader = PendingReader::start(device.clone(), old.clone());
    let blocked = reader.observe_blocked();
    if blocked {
        unittest::ktest_println!("MODEL_MMIO_PENDING: BLOCKED");
    } else {
        log::error!("MODEL_MMIO_PENDING: reader did not establish Started+Blocked+unfinished");
    }
    let (unbind, exit) = reader.stop();
    let read_result = *reader.observation.result.lock();
    let finished = reader.observation.finished.load(Ordering::Acquire);
    let unbound = domain.snapshot(&device);
    let retained_unbound = unbound.as_ref().is_some_and(|snapshot| {
        snapshot.state == DeviceState::Unclaimed
            && snapshot.driver.is_none()
            && snapshot.device.same_device(&device)
    });
    let old_closed = !old.is_available();
    let unbound_publications = publications().len();
    let rebound = reader.restore();
    drop(reader);
    let restored = is_restored(&device) && device.is_online();
    let current = publications();
    let new_publication = match current.as_slice() {
        [handle] => handle.is_available() && !handle.same_publication(&old),
        _ => false,
    };
    let mut stale_buffer = [0xa5u8; 16];
    let stale_read = if !old.is_available() {
        Some(old.read(&mut stale_buffer))
    } else {
        None
    };
    drop(current);
    drop(old);
    drop(device);
    assert!(blocked);
    assert_eq!(unbind, Ok(()));
    assert_eq!(exit, Some(0));
    assert!(finished);
    assert_eq!(
        read_result,
        Some(ReadResult {
            result: Err(kerrno::KError::BadState),
            buffer: [0xa5; 16],
        })
    );
    assert!(retained_unbound);
    assert!(old_closed);
    assert_eq!(unbound_publications, 0);
    assert_eq!(rebound, Ok(()));
    assert!(restored);
    assert!(new_publication);
    assert_eq!(stale_read, Some(Err(kerrno::KError::BadState)));
    assert_eq!(stale_buffer, [0xa5; 16]);
    unittest::ktest_println!("MODEL_MMIO_PENDING: CLOSED");
}
