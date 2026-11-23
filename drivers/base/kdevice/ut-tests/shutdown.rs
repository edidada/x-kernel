// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! System shutdown traversal tests: ordering, probe lockdown and skip rules.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use driver_base::{DeviceKind, DriverError, DriverResult};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;
use unittest::{assert_eq, def_test};

use crate::{
    BusInstance, BusTypeId, DeviceDriver, DeviceIdentity, DeviceLocation,
    DeviceState, DiscoveryOrigin, PlatformIdentity, ProbeOutcome,
    find_device, register_driver_object,
};

/// Device ids recorded by shutdown hooks, in traversal visit order.
static SHUTDOWN_VISITS: LazyInit<SpinNoPreempt<Vec<u64>>> = LazyInit::new();
/// Number of times `probe_device_desc` was refused from inside a hook.
static REFUSED_PROBES: AtomicUsize = AtomicUsize::new(0);
/// Whether the current hook observed the probe lockdown flag.
static LOCKDOWN_OBSERVED: LazyInit<SpinNoPreempt<Vec<bool>>> = LazyInit::new();

fn visits() -> &'static SpinNoPreempt<Vec<u64>> {
    SHUTDOWN_VISITS.call_once(|| SpinNoPreempt::new(Vec::new()));
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once just initialized the trace"
    )]
    SHUTDOWN_VISITS
        .get()
        .expect("shutdown visit trace is initialized on first use")
}

fn lockdown_observations() -> &'static SpinNoPreempt<Vec<bool>> {
    LOCKDOWN_OBSERVED.call_once(|| SpinNoPreempt::new(Vec::new()));
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once just initialized the trace"
    )]
    LOCKDOWN_OBSERVED
        .get()
        .expect("lockdown observation trace is initialized on first use")
}

fn reset_traces() {
    visits().lock().clear();
    lockdown_observations().lock().clear();
    REFUSED_PROBES.store(0, Ordering::SeqCst);
}

/// Driver that records its device id on shutdown and optionally probes a
/// sibling descriptor from inside the hook.
struct RecordingDriver {
    /// Descriptor id probed from inside the shutdown hook, if any.
    probe_target: Option<crate::DeviceDescId>,
}

impl DeviceDriver for RecordingDriver {
    type Data = ();
    fn name(&self) -> &'static str {
        "shutdown-recording"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn crate::DeviceMatcher {
        static M: crate::CompatibleAliasMatcher = crate::CompatibleAliasMatcher("shutdown-test");
        &M
    }

    fn probe_device(&self, _ctx: &crate::ProbeContext<'_>) -> DriverResult<()> {
        Ok(())
    }

    fn shutdown(&self, ctx: &crate::PmContext<'_>, _data: Option<&mut Self::Data>) {
        lockdown_observations()
            .lock()
            .push(crate::probes_are_blocked());
        if let Some(target) = self.probe_target
            && crate::probe_device_desc(target) == Err(DriverError::ResourceBusy)
        {
            REFUSED_PROBES.fetch_add(1, Ordering::SeqCst);
        }
        visits().lock().push(ctx.device().id().raw());
    }
}

fn reset_model() -> Arc<BusInstance> {
    let (_bus_type, bus) = crate::reset_and_setup_platform_bus_for_tests("shutdown-test-bus");
    bus
}

fn platform_desc(bus: &BusInstance, id: usize, alias: &'static str) -> crate::DeviceDesc {
    crate::device_desc_add(crate::DeviceDescSpec {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: id as u16 },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some(alias),
            firmware_id: None,
        }),
        transport: None,
        resources: crate::ResourceSet::new(),
        dma: None,
        intx: None,
        firmware_node: None,
    })
    .expect("descriptor registration succeeds")
}

#[def_test(serial)]
fn shutdown_visits_bound_devices_in_reverse_registration_order() {
    reset_traces();
    let bus = reset_model();
    let first = platform_desc(&bus, 1, "shutdown-test");
    let second = platform_desc(&bus, 2, "shutdown-test");
    let third = platform_desc(&bus, 3, "shutdown-test");
    register_driver_object(crate::BoxedDriver::new(RecordingDriver {
        probe_target: None,
    }));

    for desc in [&first, &second, &third] {
        let device = find_device(desc.device_id()).expect("device is published");
        assert!(matches!(device.state(), DeviceState::Active));
    }

    crate::shutdown_all_devices();

    assert_eq!(
        *visits().lock(),
        alloc::vec![
            third.device_id().raw(),
            second.device_id().raw(),
            first.device_id().raw()
        ],
        "shutdown must visit devices in reverse registration order"
    );
    // Shutdown leaves the binding state untouched: the terminal path never
    // reaches removal, so state must stay Active for diagnostics.
    let device = find_device(third.device_id()).expect("device survives shutdown");
    assert!(matches!(device.state(), DeviceState::Active));
}

#[def_test(serial)]
fn shutdown_refuses_probe_started_from_inside_hook() {
    reset_traces();
    let bus = reset_model();
    // A descriptor the recording driver never matches stays Pending across
    // registration, so the shutdown hook can aim a probe attempt at it.
    let hook_target = platform_desc(&bus, 7, "shutdown-other");
    register_driver_object(crate::BoxedDriver::new(RecordingDriver {
        probe_target: Some(hook_target.id()),
    }));
    // A matching descriptor, probed explicitly (post-registration descriptors
    // are not auto-probed), so the traversal actually runs one hook.
    let bound = platform_desc(&bus, 8, "shutdown-test");
    assert_eq!(
        crate::probe_device_desc(bound.id()).unwrap(),
        ProbeOutcome::Activated
    );

    assert!(!crate::probes_are_blocked());
    crate::shutdown_all_devices();

    assert_eq!(
        REFUSED_PROBES.load(Ordering::SeqCst),
        1,
        "a probe attempted from inside a shutdown hook must be refused with ResourceBusy"
    );
    assert_eq!(
        *lockdown_observations().lock(),
        alloc::vec![true],
        "hooks must observe the probe lockdown during traversal"
    );
    assert!(!crate::probes_are_blocked(), "lockdown must be released");
    assert_eq!(crate::active_probe_count(), 0);

    // The refused descriptor stays Pending and probes normally afterwards
    // (Unmatched here: the recording driver never matches its alias).
    let outcome = crate::probe_device_desc(hook_target.id()).unwrap();
    assert_eq!(outcome, ProbeOutcome::Unmatched);
}

#[def_test(serial)]
fn probes_resume_after_traversal() {
    reset_traces();
    let bus = reset_model();
    let before = platform_desc(&bus, 11, "shutdown-test");
    register_driver_object(crate::BoxedDriver::new(RecordingDriver {
        probe_target: None,
    }));
    crate::shutdown_all_devices();
    assert_eq!(visits().lock().len(), 1);
    let _ = before;

    assert!(!crate::probes_are_blocked());
    let after = platform_desc(&bus, 12, "shutdown-test");
    let outcome = crate::probe_device_desc(after.id()).unwrap();
    assert_eq!(outcome, ProbeOutcome::Activated);
}
