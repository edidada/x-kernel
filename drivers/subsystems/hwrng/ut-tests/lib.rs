// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for the hardware RNG device contract.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use kspin::SpinNoIrq;
use unittest::{assert_eq, def_test};

use super::{completion::*, *};

struct ImmediateDrain;

impl RngDrain for ImmediateDrain {
    fn wait_until(&self, drained: &mut dyn FnMut() -> bool) -> KResult {
        core::assert!(drained());
        Ok(())
    }

    fn notify(&self) {}
}

struct ScriptedCompletion {
    error: Option<KError>,
    produce: Box<dyn Fn() + Send + Sync>,
}

impl RngCompletion for ScriptedCompletion {
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> KResult {
        if let Some(error) = self.error {
            return Err(error);
        }
        if !ready() {
            (self.produce)();
            if !ready() {
                return Err(KError::InvalidInput);
            }
        }
        Ok(())
    }

    fn notify(&self) {} // The scripted host has no parked tasks.
}

struct TestDevice {
    bytes: Arc<SpinNoIrq<Vec<u8>>>,
    completion: Box<dyn RngCompletion>,
}
impl HwrngDevice for TestDevice {
    fn is_failed(&self) -> bool {
        false
    }

    fn read(&self, buf: &mut [u8]) -> KResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut result = Err(KError::WouldBlock);
        self.completion.wait_until(&mut || {
            let mut bytes = self.bytes.lock();
            if bytes.is_empty() {
                result = Err(KError::WouldBlock);
                return false;
            }
            let count = buf.len().min(bytes.len());
            buf[..count].copy_from_slice(&bytes[..count]);
            bytes.drain(..count);
            result = Ok(count);
            result != Err(KError::WouldBlock)
        })?;
        result
    }
}

fn device(error: Option<KError>) -> TestDevice {
    let bytes = Arc::new(SpinNoIrq::new(Vec::new()));
    let completion_bytes = bytes.clone();
    TestDevice {
        bytes,
        completion: Box::new(ScriptedCompletion {
            error,
            produce: Box::new(move || completion_bytes.lock().extend_from_slice(&[7, 9])),
        }),
    }
}

#[def_test]
fn consumer_reads_short_data_through_device_contract() {
    let device = device(None);
    let source: &dyn HwrngDevice = &device;
    let mut buf = [0xcc; 4];
    assert_eq!(source.read(&mut buf), Ok(2));
    assert_eq!(buf, [7, 9, 0xcc, 0xcc]);
}

#[def_test]
fn waiting_errors_do_not_consume_bytes() {
    for error in [KError::InvalidInput, KError::NoMemory] {
        let device = device(Some(error));
        device.bytes.lock().push(17);
        let mut buf = [0xcc];
        assert_eq!(device.read(&mut []), Ok(0));
        assert_eq!(device.read(&mut buf), Err(error));
        assert_eq!(buf, [0xcc]);
        assert_eq!(&*device.bytes.lock(), &[17]);
    }
}

#[def_test(serial)]
fn withdrawn_consumer_cannot_access_driver_payload() {
    use core::sync::atomic::{AtomicUsize, Ordering};

    static CLASS: HwrngRegistry = HwrngRegistry::new();
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    struct Probe;
    impl HwrngDevice for Probe {
        fn read(&self, _buf: &mut [u8]) -> KResult<usize> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }

        fn is_failed(&self) -> bool {
            CALLS.fetch_add(1, Ordering::SeqCst);
            false
        }
    }
    CALLS.store(0, Ordering::SeqCst);
    let mut registration = CLASS
        .attach_legacy(
            "test-rng",
            alloc::string::String::from("test"),
            Arc::new(Probe),
            Arc::new(ImmediateDrain),
        )
        .unwrap();
    let consumer = registration.publication();
    assert_eq!(consumer.is_failed(), Ok(false));
    assert_eq!(consumer.read(&mut []), Ok(0));
    assert_eq!(registration.unregister(), Ok(()));
    assert_eq!(registration.unregister(), Ok(()));
    assert_eq!(consumer.is_failed(), Err(KError::BadState));
    assert_eq!(consumer.read(&mut []), Err(KError::BadState));
    assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    assert!(CLASS.devices().is_empty());
}

#[def_test(serial)]
fn preparation_failure_does_not_invoke_predicate_or_close_registration() {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Drain {
        reject: AtomicBool,
        checks: AtomicUsize,
    }
    impl RngDrain for Drain {
        fn wait_until(&self, drained: &mut dyn FnMut() -> bool) -> KResult {
            if self.reject.load(Ordering::Acquire) {
                return Err(KError::InvalidInput);
            }
            self.checks.fetch_add(1, Ordering::Relaxed);
            core::assert!(drained());
            Ok(())
        }

        fn notify(&self) {}
    }
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let drain = Arc::new(Drain {
        reject: AtomicBool::new(true),
        checks: AtomicUsize::new(0),
    });
    let mut registration = REGISTRY
        .attach_legacy(
            "prepare",
            "prepare".into(),
            Arc::new(device(None)),
            drain.clone(),
        )
        .unwrap();
    let handle = registration.publication();
    assert_eq!(registration.unregister(), Err(KError::InvalidInput));
    assert_eq!(drain.checks.load(Ordering::Relaxed), 0);
    assert!(handle.is_available());
    assert_eq!(REGISTRY.devices().len(), 1);
    drain.reject.store(false, Ordering::Release);
    assert_eq!(registration.unregister(), Ok(()));
    assert_eq!(drain.checks.load(Ordering::Relaxed), 1);
    assert!(!handle.is_available());
    assert!(REGISTRY.devices().is_empty());
}

#[def_test(serial)]
fn forgotten_credential_retains_observable_runtime_without_cleanup() {
    use core::sync::atomic::{AtomicUsize, Ordering};
    struct Runtime(Arc<AtomicUsize>);
    impl Drop for Runtime {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    impl HwrngDevice for Runtime {
        fn read(&self, _: &mut [u8]) -> KResult<usize> {
            Ok(0)
        }

        fn is_failed(&self) -> bool {
            false
        }
    }
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let notifications = Arc::new(AtomicUsize::new(0));
    REGISTRY.subscribe_removed(Arc::new({
        let notifications = notifications.clone();
        move |_| {
            notifications.fetch_add(1, Ordering::Relaxed);
        }
    }));
    let registration = REGISTRY
        .attach_legacy(
            "forgotten",
            "forgotten".into(),
            Arc::new(Runtime(drops.clone())),
            Arc::new(ImmediateDrain),
        )
        .unwrap();
    let handle = registration.publication();
    drop(registration);
    assert!(handle.is_available());
    drop(handle);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert_eq!(notifications.load(Ordering::Relaxed), 0);
    assert_eq!(REGISTRY.devices().len(), 1);
    assert_eq!(REGISTRY.devices()[0].read(&mut []), Ok(0));
}

#[def_test(serial)]
fn registrations_are_independent_and_old_handles_do_not_reopen() {
    static REGISTRY: HwrngRegistry = HwrngRegistry::new();
    let runtime = Arc::new(device(None));
    let mut first = REGISTRY
        .attach_legacy(
            "same-driver",
            "same-path".into(),
            runtime.clone(),
            Arc::new(ImmediateDrain),
        )
        .unwrap();
    let mut sibling = REGISTRY
        .attach_legacy(
            "same-driver",
            "same-path".into(),
            runtime.clone(),
            Arc::new(ImmediateDrain),
        )
        .unwrap();
    let old = first.publication();
    assert!(!old.same_publication(&sibling.publication()));
    assert_eq!(REGISTRY.devices().len(), 2);
    assert_eq!(first.unregister(), Ok(()));
    assert_eq!(old.is_failed(), Err(KError::BadState));
    assert_eq!(sibling.publication().is_failed(), Ok(false));
    let mut replacement = REGISTRY
        .attach_legacy(
            "same-driver",
            "same-path".into(),
            runtime,
            Arc::new(ImmediateDrain),
        )
        .unwrap();
    assert!(!old.same_publication(&replacement.publication()));
    assert_eq!(old.read(&mut []), Err(KError::BadState));
    assert_eq!(replacement.publication().read(&mut []), Ok(0));
    assert_eq!(sibling.unregister(), Ok(()));
    assert_eq!(replacement.unregister(), Ok(()));
    assert!(REGISTRY.devices().is_empty());
}
