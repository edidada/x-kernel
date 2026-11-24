// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Basic deferred-probe lifecycle through the real registry and workqueue.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use ksync::Semaphore;

use driver_base::{DeviceKind, DriverError, DriverResult};
use kdevice::{
    ActiveDeviceAdoption, BusTypeId, CompatibleAliasMatcher, DeviceDesc, DeviceDescSpec,
    DeviceDriver, DeviceIdentity, DeviceLocation, DeviceMatcher, DiscoveryOrigin, PlatformIdentity,
    ProbeContext, ProbeOutcome, ResourceSet, adopt_active_device, device_desc_add, find_device,
    probe_device_desc, register_driver_object, remove_device_managed,
    reset_and_setup_platform_bus_for_tests, unregister_device_desc,
};
use unittest::{assert, assert_eq, def_test};

const WAIT_YIELDS: usize = 4_000_000;

type ProbeFn = dyn for<'a> Fn(&ProbeContext<'a>) -> DriverResult<()> + Send + Sync;

struct DeferredDriver {
    name: &'static str,
    matcher: CompatibleAliasMatcher,
    probe: Arc<ProbeFn>,
}

impl DeviceDriver for DeferredDriver {
    type Data = ();

    fn name(&self) -> &'static str {
        self.name
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &self.matcher
    }

    fn probe_device(&self, ctx: &ProbeContext<'_>) -> DriverResult<()> {
        (self.probe)(ctx)
    }
}

fn driver(
    name: &'static str,
    alias: &'static str,
    probe: impl for<'a> Fn(&ProbeContext<'a>) -> DriverResult<()> + Send + Sync + 'static,
) -> kdevice::BoxedDriver {
    kdevice::BoxedDriver::new(DeferredDriver {
        name,
        matcher: CompatibleAliasMatcher(alias),
        probe: Arc::new(probe),
    })
}

fn desc(bus_id: kdevice::BusId, alias: &'static str, location: u16) -> DeviceDesc {
    device_desc_add(DeviceDescSpec {
        bus_id,
        parent: None,
        location: DeviceLocation::PlatformStatic { id: location },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some(alias),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        intx: None,
        firmware_node: None,
    })
    .unwrap()
}

fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..WAIT_YIELDS {
        if predicate() {
            return true;
        }
        ktask::yield_now();
    }
    false
}

fn wait_permit(semaphore: &Semaphore) -> bool {
    for _ in 0..4_000_000 {
        if semaphore.try_acquire() {
            return true;
        }
        ktask::yield_now();
    }
    false
}

fn begin_deferred_test(name: &'static str) -> Arc<kdevice::BusInstance> {
    crate::deferred::init();
    // Cancel the previous fixture's requests before its descriptors can be
    // reused, then wait for the real work callback to drain.
    kdevice::cancel_all_deferred_for_tests();
    crate::deferred::flush_for_tests();
    core::assert!(
        wait_until(|| {
            kdevice::deferred_test_hooks::is_idle()
                && !kdevice::deferred_test_hooks::is_drain_running()
        }),
        "deferred drain must be quiescent before resetting test state"
    );
    kdevice::reset_deferred_for_tests();
    let (_bus_type, bus) = reset_and_setup_platform_bus_for_tests(name);
    bus
}

fn cleanup(descs: &[DeviceDesc]) {
    for desc in descs {
        let _ = unregister_device_desc(desc.id());
    }
}

#[def_test(serial)]
fn consumer_defers_then_supplier_success_wakes_it() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-basic-bus");

    let ready = Arc::new(AtomicBool::new(false));
    let consumer_ready = ready.clone();
    let consumer_driver = register_driver_object(driver(
        "deferred-consumer",
        "deferred-consumer",
        move |_| {
            if consumer_ready.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(DriverError::Deferred)
            }
        },
    ));
    let supplier_ready = ready.clone();
    let _supplier_driver = register_driver_object(driver(
        "deferred-supplier",
        "deferred-supplier",
        move |_| {
            supplier_ready.store(true, Ordering::SeqCst);
            Ok(())
        },
    ));

    let consumer_desc = desc(bus.id(), "deferred-consumer", 0x8100);
    let supplier_desc = desc(bus.id(), "deferred-supplier", 0x8101);
    assert_eq!(
        probe_device_desc(consumer_desc.id()).unwrap(),
        ProbeOutcome::Deferred
    );
    assert_eq!(
        find_device(consumer_desc.device_id()).unwrap().state(),
        kdevice::DeviceState::Discovered
    );
    // Registering the unrelated supplier must not clear the old wait.
    assert!(!kdevice::deferred_test_hooks::is_idle());
    let stats = consumer_driver.probe_stats();
    assert_eq!(stats.attempts, 1);
    assert_eq!(stats.failures, 0);
    assert_eq!(stats.deferred, 1);

    assert_eq!(
        probe_device_desc(supplier_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert!(
        wait_until(|| {
            find_device(consumer_desc.device_id())
                .is_some_and(|device| device.state() == kdevice::DeviceState::Active)
        }),
        "supplier success must wake the deferred consumer"
    );
    assert_eq!(
        probe_device_desc(consumer_desc.id()).unwrap(),
        ProbeOutcome::Skipped
    );
    assert!(
        wait_until(|| !kdevice::deferred_test_hooks::is_drain_running()),
        "drain must finish before the test releases its fixture"
    );
    cleanup(&[consumer_desc, supplier_desc]);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn supplier_first_leaves_no_pending_request() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-supplier-first-bus");

    let ready = Arc::new(AtomicBool::new(false));
    let supplier_ready = ready.clone();
    let _supplier = register_driver_object(driver(
        "deferred-first-supplier",
        "deferred-first-supplier",
        move |_| {
            supplier_ready.store(true, Ordering::SeqCst);
            Ok(())
        },
    ));
    let consumer_ready = ready.clone();
    let _consumer = register_driver_object(driver(
        "deferred-first-consumer",
        "deferred-first-consumer",
        move |_| {
            if consumer_ready.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(DriverError::Deferred)
            }
        },
    ));

    let supplier_desc = desc(bus.id(), "deferred-first-supplier", 0x8110);
    let consumer_desc = desc(bus.id(), "deferred-first-consumer", 0x8111);
    assert_eq!(
        probe_device_desc(supplier_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert_eq!(
        probe_device_desc(consumer_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );

    // No later success event recreates a binding for an already active device.
    assert_eq!(
        probe_device_desc(consumer_desc.id()).unwrap(),
        ProbeOutcome::Skipped
    );
    assert!(wait_until(|| !kdevice::deferred_test_hooks::is_drain_running()));
    cleanup(&[consumer_desc, supplier_desc]);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn repeated_defer_does_not_self_trigger() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-repeat-bus");

    let attempts = Arc::new(AtomicUsize::new(0));
    let consumer_attempts = attempts.clone();
    let _consumer = register_driver_object(driver(
        "deferred-repeat-consumer",
        "deferred-repeat-consumer",
        move |_| {
            consumer_attempts.fetch_add(1, Ordering::SeqCst);
            Err(DriverError::Deferred)
        },
    ));
    let _supplier = register_driver_object(driver(
        "deferred-repeat-supplier",
        "deferred-repeat-supplier",
        |_| Ok(()),
    ));

    let consumer_desc = desc(bus.id(), "deferred-repeat-consumer", 0x8120);
    let supplier_desc = desc(bus.id(), "deferred-repeat-supplier", 0x8121);
    assert_eq!(
        probe_device_desc(consumer_desc.id()).unwrap(),
        ProbeOutcome::Deferred
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    assert_eq!(
        probe_device_desc(supplier_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert!(
        wait_until(|| attempts.load(Ordering::SeqCst) >= 2),
        "the first success event must retry the deferred consumer once"
    );
    // The drain barrier below proves no self-triggered follow-up was queued;
    // the count must stay at 2 without a fixed yield heuristic.
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    // A second success event is a new trigger and may retry once more.
    let second_supplier = register_driver_object(driver(
        "deferred-repeat-supplier-2",
        "deferred-repeat-supplier-2",
        |_| Ok(()),
    ));
    let second_desc = desc(bus.id(), "deferred-repeat-supplier-2", 0x8122);
    assert_eq!(
        probe_device_desc(second_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert!(
        wait_until(|| attempts.load(Ordering::SeqCst) >= 3),
        "a later success event must trigger another retry"
    );
    let _ = second_supplier;

    assert!(
        wait_until(|| !kdevice::deferred_test_hooks::is_drain_running()),
        "drain must finish before the test releases its fixture"
    );
    assert_eq!(
        kdevice::deferred_test_hooks::max_concurrent_drains(),
        1,
        "the single drain work must never run concurrently"
    );
    cleanup(&[consumer_desc, supplier_desc, second_desc]);
    unittest::TestResult::Ok
}

fn ktask_yield() {
    for _ in 0..64 {
        ktask::yield_now();
    }
}

#[def_test(serial)]
fn explicit_takeover_invalidates_claimed_worker() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-claimed-bus");

    let attempts = Arc::new(AtomicUsize::new(0));
    let consumer_attempts = attempts.clone();
    let _consumer = register_driver_object(driver(
        "deferred-claimed-consumer",
        "deferred-claimed-consumer",
        move |_| {
            let attempt = consumer_attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == 1 {
                Err(DriverError::Deferred)
            } else {
                Err(DriverError::Io)
            }
        },
    ));
    let _supplier = register_driver_object(driver(
        "deferred-claimed-supplier",
        "deferred-claimed-supplier",
        |_| Ok(()),
    ));

    let consumer_desc = desc(bus.id(), "deferred-claimed-consumer", 0x8130);
    let supplier_desc = desc(bus.id(), "deferred-claimed-supplier", 0x8131);
    assert_eq!(
        probe_device_desc(consumer_desc.id()).unwrap(),
        ProbeOutcome::Deferred
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);

    kdevice::deferred_test_hooks::arm_pause_after_claim();
    assert_eq!(
        probe_device_desc(supplier_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert!(
        kdevice::deferred_test_hooks::wait_until_paused(),
        "worker must claim the deferred request before takeover"
    );

    // Explicit probe takes over while the old worker is paused after claiming.
    // It fails normally, so the old worker must not probe again or rebuild a
    // wait for the superseded request.
    assert_eq!(
        probe_device_desc(consumer_desc.id()).unwrap(),
        ProbeOutcome::Unclaimed
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    kdevice::deferred_test_hooks::release_pause();
    assert!(
        wait_until(|| !kdevice::deferred_test_hooks::is_drain_running()),
        "paused drain must finish after release"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        find_device(consumer_desc.device_id()).unwrap().state(),
        kdevice::DeviceState::Discovered
    );

    cleanup(&[consumer_desc, supplier_desc]);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn unregister_pending_cancels_deferred_request() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-unregister-bus");
    let _consumer = register_driver_object(driver(
        "deferred-unregister-consumer",
        "deferred-unregister-consumer",
        |_| Err(DriverError::Deferred),
    ));
    let consumer_desc = desc(bus.id(), "deferred-unregister-consumer", 0x8140);
    assert_eq!(
        probe_device_desc(consumer_desc.id()).unwrap(),
        ProbeOutcome::Deferred
    );
    assert!(!kdevice::deferred_test_hooks::is_idle());

    unregister_device_desc(consumer_desc.id()).unwrap();
    assert!(
        kdevice::deferred_test_hooks::is_idle(),
        "unregistering a Pending descriptor must cancel its deferred request: {:?}",
        (
            kdevice::deferred_test_hooks::debug_counts(),
            kdevice::deferred_test_hooks::pending_desc()
        )
    );
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn chain_of_deferred_consumers_advances_through_successes() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-chain-bus");

    let a_ready = Arc::new(AtomicBool::new(false));
    let b_ready = Arc::new(AtomicBool::new(false));
    let c_ready = Arc::new(AtomicBool::new(false));
    let a = a_ready.clone();
    let b = b_ready.clone();
    let c = c_ready.clone();
    let a_after_b = a_ready.clone();
    let unrelated_probes = Arc::new(AtomicUsize::new(0));
    let unrelated_count = unrelated_probes.clone();
    let _a_driver = register_driver_object(driver(
        "deferred-chain-a",
        "deferred-chain-a",
        move |_| {
            if a.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(DriverError::Deferred)
            }
        },
    ));
    let _b_driver = register_driver_object(driver(
        "deferred-chain-b",
        "deferred-chain-b",
        move |_| {
            if b.load(Ordering::SeqCst) {
                a_after_b.store(true, Ordering::SeqCst);
                Ok(())
            } else {
                Err(DriverError::Deferred)
            }
        },
    ));
    let _unrelated_driver = register_driver_object(driver(
        "deferred-chain-unrelated",
        "deferred-chain-unrelated",
        move |_| {
            unrelated_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    ));
    let _c_driver = register_driver_object(driver(
        "deferred-chain-c",
        "deferred-chain-c",
        move |_| {
            c.store(true, Ordering::SeqCst);
            Ok(())
        },
    ));

    let a_desc = desc(bus.id(), "deferred-chain-a", 0x8150);
    let b_desc = desc(bus.id(), "deferred-chain-b", 0x8151);
    let c_desc = desc(bus.id(), "deferred-chain-c", 0x8152);
    let unrelated_desc = desc(bus.id(), "deferred-chain-unrelated", 0x8153);
    assert_eq!(
        probe_device_desc(unrelated_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert_eq!(unrelated_probes.load(Ordering::SeqCst), 1);
    assert_eq!(probe_device_desc(a_desc.id()).unwrap(), ProbeOutcome::Deferred);
    assert_eq!(probe_device_desc(b_desc.id()).unwrap(), ProbeOutcome::Deferred);
    // C success sets B ready and triggers the pending A/B batch.
    b_ready.store(true, Ordering::SeqCst);
    assert_eq!(probe_device_desc(c_desc.id()).unwrap(), ProbeOutcome::Activated);
    // B's success sets A ready; A then completes.
    assert!(
        wait_until(|| {
            find_device(a_desc.device_id()).is_some_and(|device| {
                device.state() == kdevice::DeviceState::Active
            })
        }),
        "A must be retried after B succeeds"
    );
    assert!(b_ready.load(Ordering::SeqCst));
    assert!(c_ready.load(Ordering::SeqCst));
    assert_eq!(
        unrelated_probes.load(Ordering::SeqCst),
        1,
        "an already active unrelated device must not be reprobed"
    );
    cleanup(&[a_desc, b_desc, c_desc, unrelated_desc]);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn cyclic_deferred_consumers_stay_quiescent() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-cycle-bus");
    let counts = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
    for (index, alias) in [(0, "deferred-cycle-a"), (1, "deferred-cycle-b")] {
        let count = counts[index].clone();
        let _driver = register_driver_object(driver(
            alias,
            alias,
            move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Err(DriverError::Deferred)
            },
        ));
    }
    let a_desc = desc(bus.id(), "deferred-cycle-a", 0x8160);
    let b_desc = desc(bus.id(), "deferred-cycle-b", 0x8161);
    assert_eq!(probe_device_desc(a_desc.id()).unwrap(), ProbeOutcome::Deferred);
    assert_eq!(probe_device_desc(b_desc.id()).unwrap(), ProbeOutcome::Deferred);
    let _supplier = register_driver_object(driver(
        "deferred-cycle-supplier",
        "deferred-cycle-supplier",
        |_| Ok(()),
    ));
    let supplier_desc = desc(bus.id(), "deferred-cycle-supplier", 0x8162);
    assert_eq!(
        probe_device_desc(supplier_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert!(wait_until(|| {
        counts.iter().all(|count| count.load(Ordering::SeqCst) >= 2)
    }));
    ktask_yield();
    assert_eq!(counts[0].load(Ordering::SeqCst), 2);
    assert_eq!(counts[1].load(Ordering::SeqCst), 2);
    cleanup(&[a_desc, b_desc, supplier_desc]);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn only_scan_deferred_replaces_old_wait() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-only-bus");
    let _a = register_driver_object(driver(
        "deferred-only-a",
        "deferred-only-shared",
        |_| Err(DriverError::Deferred),
    ));
    let consumer_desc = desc(bus.id(), "deferred-only-shared", 0x8170);
    assert_eq!(
        probe_device_desc(consumer_desc.id()).unwrap(),
        ProbeOutcome::Deferred
    );
    let old_candidate = kdevice::deferred_test_hooks::pending_candidate().unwrap();

    let b = register_driver_object(driver(
        "deferred-only-b",
        "deferred-only-shared",
        |_| Err(DriverError::Deferred),
    ));
    assert_ne!(old_candidate, b.id().raw());
    assert_eq!(
        kdevice::deferred_test_hooks::pending_candidate(),
        Some(b.id().raw()),
        "Only-scan Deferred must replace the old pending candidate"
    );
    cleanup(&[consumer_desc]);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn adoption_success_wakes_pending_consumer() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-adopt-bus");
    let ready = Arc::new(AtomicBool::new(false));
    let consumer_ready = ready.clone();
    let _consumer = register_driver_object(driver(
        "deferred-adopt-consumer",
        "deferred-adopt-consumer",
        move |_| {
            if consumer_ready.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(DriverError::Deferred)
            }
        },
    ));
    let consumer_desc = desc(bus.id(), "deferred-adopt-consumer", 0x8180);
    assert_eq!(
        probe_device_desc(consumer_desc.id()).unwrap(),
        ProbeOutcome::Deferred
    );
    ready.store(true, Ordering::SeqCst);

    let adoption_driver = register_driver_object(kdevice::BoxedDriver::new_adoptable(
        DeferredDriver {
            name: "deferred-adopt-source",
            matcher: CompatibleAliasMatcher("deferred-adopt-source"),
            probe: Arc::new(|_| Ok(())),
        },
    ));
    let adopted = adopt_active_device(ActiveDeviceAdoption {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: 0x8181 },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("deferred-adopt-source"),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        driver: adoption_driver,
    })
    .unwrap();
    assert!(
        wait_until(|| {
            find_device(consumer_desc.device_id()).is_some_and(|device| {
                device.state() == kdevice::DeviceState::Active
            })
        }),
        "successful adoption must advance the pending consumer"
    );
    cleanup(&[consumer_desc]);
    let _ = kdevice::remove_device_managed(adopted.id());
    unittest::TestResult::Ok
}


#[def_test(serial)]
fn busy_removal_does_not_cancel_pending_state() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-busy-bus");
    let _driver = register_driver_object(driver(
        "deferred-busy-device",
        "deferred-busy-device",
        |_| Ok(()),
    ));
    let device_desc = desc(bus.id(), "deferred-busy-device", 0x81a0);
    assert_eq!(
        probe_device_desc(device_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    let device = find_device(device_desc.device_id()).unwrap();
    let binding = device.binding_ref().expect("active binding");
    let guard = binding.try_acquire(&device).expect("active use guard");
    let before = kdevice::deferred_test_hooks::cancel_count();
    assert_eq!(
        remove_device_managed(device.id()),
        Err(DriverError::ResourceBusy)
    );
    assert_eq!(
        kdevice::deferred_test_hooks::cancel_count(),
        before,
        "an uncommitted ResourceBusy removal must not cancel deferred state"
    );
    drop(guard);
    remove_device_managed(device.id()).unwrap();
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn deferred_cleanup_runs_once_per_attempt() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-cleanup-bus");
    let ready = Arc::new(AtomicBool::new(false));
    let cleanups = Arc::new(AtomicUsize::new(0));
    let consumer_ready = ready.clone();
    let consumer_cleanups = cleanups.clone();
    let _consumer = register_driver_object(driver(
        "deferred-cleanup-consumer",
        "deferred-cleanup-consumer",
        move |ctx| {
            let count = consumer_cleanups.clone();
            ctx.add_cleanup(move || {
                count.fetch_add(1, Ordering::SeqCst);
            })?;
            if consumer_ready.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(DriverError::Deferred)
            }
        },
    ));
    let supplier_ready = ready.clone();
    let _supplier = register_driver_object(driver(
        "deferred-cleanup-supplier",
        "deferred-cleanup-supplier",
        move |_| {
            supplier_ready.store(true, Ordering::SeqCst);
            Ok(())
        },
    ));
    let consumer_desc = desc(bus.id(), "deferred-cleanup-consumer", 0x81b0);
    let supplier_desc = desc(bus.id(), "deferred-cleanup-supplier", 0x81b1);
    assert_eq!(
        probe_device_desc(consumer_desc.id()).unwrap(),
        ProbeOutcome::Deferred
    );
    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(
        probe_device_desc(supplier_desc.id()).unwrap(),
        ProbeOutcome::Activated
    );
    assert!(wait_until(|| {
        find_device(consumer_desc.device_id())
            .is_some_and(|device| device.state() == kdevice::DeviceState::Active)
    }));
    assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    remove_device_managed(consumer_desc.device_id()).unwrap();
    assert_eq!(
        cleanups.load(Ordering::SeqCst),
        2,
        "failed attempt cleanup and successful retry cleanup must each run once"
    );
    cleanup(&[consumer_desc, supplier_desc]);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn executor_backlog_contract_is_isolated() -> unittest::TestResult {
    assert!(
        kdevice::deferred_backlog_contract_for_test(),
        "pending must be accepted before executor install and promoted by install"
    );
    assert!(
        kdevice::deferred_promote_order_for_test(),
        "existing active requests must precede promoted pending requests"
    );
    unittest::TestResult::Ok
}


fn commit_hook_case(point: kdevice::deferred_test_hooks::CommitPause) -> bool {
    let bus = begin_deferred_test("deferred-commit-hook-bus");
    let ready = Arc::new(AtomicBool::new(false));
    let attempts = Arc::new(AtomicUsize::new(0));
    let consumer_ready = ready.clone();
    let consumer_attempts = attempts.clone();
    let _consumer = register_driver_object(driver(
        "deferred-hook-consumer",
        "deferred-hook-consumer",
        move |_| {
            consumer_attempts.fetch_add(1, Ordering::SeqCst);
            if consumer_ready.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(DriverError::Deferred)
            }
        },
    ));
    let supplier_ready = ready.clone();
    let _supplier = register_driver_object(driver(
        "deferred-hook-supplier",
        "deferred-hook-supplier",
        move |_| {
            supplier_ready.store(true, Ordering::SeqCst);
            Ok(())
        },
    ));
    let consumer_desc = desc(bus.id(), "deferred-hook-consumer", 0x81c0);
    let supplier_desc = desc(bus.id(), "deferred-hook-supplier", 0x81c1);
    let consumer_id = consumer_desc.id();
    let consumer_device = consumer_desc.device_id();
    let supplier_id = supplier_desc.id();

    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (hook_entered, hook_release) = (entered.clone(), release.clone());
    kdevice::deferred_test_hooks::install_commit_hook(
        consumer_id,
        point,
        Arc::new(move || {
            hook_entered.release();
            hook_release.acquire();
        }),
    );
    let first = Arc::new(AtomicI32::new(-1));
    let first_result = first.clone();
    let task = ktask::spawn(move || {
        let code = match probe_device_desc(consumer_id) {
            Ok(ProbeOutcome::Deferred) => 0,
            Ok(_) => 1,
            Err(_) => 2,
        };
        first_result.store(code, Ordering::SeqCst);
    });
    let hit = wait_permit(&entered);
    if !hit {
        release.release();
        let _ = task.join();
        kdevice::deferred_test_hooks::clear_commit_hook();
        cleanup(&[consumer_desc, supplier_desc]);
        return false;
    }
    let supplier = probe_device_desc(supplier_id) == Ok(ProbeOutcome::Activated);
    release.release();
    let joined = task.join();
    let first_deferred = first.load(Ordering::SeqCst) == 0;
    kdevice::deferred_test_hooks::clear_commit_hook();
    let active = wait_until(|| {
        find_device(consumer_device)
            .is_some_and(|device| device.state() == kdevice::DeviceState::Active)
    });
    let no_duplicate = attempts.load(Ordering::SeqCst) == 2;
    cleanup(&[consumer_desc, supplier_desc]);
    joined == 0 && first_deferred && supplier && active && no_duplicate
}

#[def_test(serial)]
fn trigger_before_commit_is_not_lost() -> unittest::TestResult {
    assert!(
        commit_hook_case(kdevice::deferred_test_hooks::CommitPause::BeforeCommit),
        "trigger before commit must still retry"
    );
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn trigger_after_commit_is_not_lost() -> unittest::TestResult {
    assert!(
        commit_hook_case(kdevice::deferred_test_hooks::CommitPause::AfterCommit),
        "trigger after commit before kick must still retry"
    );
    unittest::TestResult::Ok
}

#[cfg(feature = "char")]
mod b11 {
    use alloc::{boxed::Box, string::String, sync::Arc};

    use driver_base::{Device, DeviceKind, DriverError, DriverResult};
    use kclass::CharDevice;
    use kdevice::{ProbeContext, ProbeOutcome};
    use kspin::SpinNoPreempt;
    use unittest::{assert_eq, def_test};

    use super::{
        WAIT_YIELDS, begin_deferred_test, cleanup, desc, driver, find_device,
        probe_device_desc, register_driver_object,
        wait_until,
    };

    struct TestChar {
        name: String,
    }

    impl Device for TestChar {
        fn name(&self) -> &str {
            &self.name
        }

        fn device_kind(&self) -> DeviceKind {
            DeviceKind::Char
        }
    }

    impl CharDevice for TestChar {
        fn read(&self, _buf: &mut [u8]) -> DriverResult<usize> {
            Ok(0)
        }

        fn write(&self, buf: &[u8]) -> DriverResult<usize> {
            Ok(buf.len())
        }
    }

    #[def_test(serial)]
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
    )]
    #[allow(
        unknown_lints,
        std_box_in_kernel_tests,
        reason = "CG-049-no-std-box-in-kernel-tests: kclass publish_char requires the production Box<dyn CharDevice> CharDeviceImpl"
    )]
    fn deferred_publication_is_retired_and_retry_uses_new_binding() -> unittest::TestResult {
        let bus = begin_deferred_test("deferred-b11-bus");
        kclass::reinstall_event_bridge_for_tests();

        let ready = Arc::new(core::sync::atomic::AtomicBool::new(false));
        let (fresh, old_cell) = (ready.clone(), Arc::new(SpinNoPreempt::new(None)));
        let cell = old_cell.clone();
        let _consumer = register_driver_object(driver(
            "deferred-b11-consumer",
            "deferred-b11-consumer",
            move |ctx: &ProbeContext<'_>| {
                let publication = kclass::publish_char(
                    ctx,
                    Box::new(TestChar {
                        name: String::from("deferred-b11"),
                    }),
                )?;
                *cell.lock() = Some(publication);
                if fresh.load(core::sync::atomic::Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(DriverError::Deferred)
                }
            },
        ));
        let supplier_ready = ready.clone();
        let _supplier = register_driver_object(driver(
            "deferred-b11-supplier",
            "deferred-b11-supplier",
            move |_| {
                supplier_ready.store(true, core::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        ));

        let consumer_desc = desc(bus.id(), "deferred-b11-consumer", 0x81d0);
        let supplier_desc = desc(bus.id(), "deferred-b11-supplier", 0x81d1);
        assert_eq!(
            probe_device_desc(consumer_desc.id()).unwrap(),
            ProbeOutcome::Deferred
        );
        let old = old_cell.lock().take().expect("first publication");
        assert_eq!(old.publication_state_name(), "Revoked");
        assert!(!old.is_available(), "deferred rollback must retire publication");

        assert_eq!(
            probe_device_desc(supplier_desc.id()).unwrap(),
            ProbeOutcome::Activated
        );
        assert!(wait_until(|| {
            find_device(consumer_desc.device_id())
                .is_some_and(|device| device.state() == kdevice::DeviceState::Active)
        }));
        let new = old_cell.lock().take().expect("retry publication");
        assert!(
            wait_until(|| new.is_available()),
            "retry publication must become available"
        );
        assert!(!old.is_available(), "old publication must not revive");
        assert!(!old.binding_ref().same_binding(new.binding_ref()));

        assert_eq!(
            kdevice::remove_device_managed(consumer_desc.device_id()),
            Ok(())
        );
        assert_eq!(new.publication_state_name(), "Revoked");
        assert!(!new.is_available(), "removal must retire the retry publication");
        cleanup(&[consumer_desc, supplier_desc]);
        let _ = WAIT_YIELDS;
        unittest::TestResult::Ok
    }
}

#[def_test(serial)]
fn deferring_candidate_does_not_starve_later_driver() -> unittest::TestResult {
    let bus = begin_deferred_test("deferred-candidate-fallback-bus");
    let _first = register_driver_object(driver(
        "deferred-candidate-first",
        "deferred-candidate-shared",
        |_| Err(DriverError::Deferred),
    ));
    let _second = register_driver_object(driver(
        "deferred-candidate-second",
        "deferred-candidate-shared",
        |_| Ok(()),
    ));
    let device_desc = desc(bus.id(), "deferred-candidate-shared", 0x81e0);
    assert_eq!(
        probe_device_desc(device_desc.id()).unwrap(),
        ProbeOutcome::Activated,
        "a later matching driver must still bind after an earlier candidate defers"
    );
    assert_eq!(
        find_device(device_desc.device_id()).unwrap().state(),
        kdevice::DeviceState::Active
    );
    cleanup(&[device_desc]);
    unittest::TestResult::Ok
}
