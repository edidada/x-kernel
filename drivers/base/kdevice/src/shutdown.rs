// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! System shutdown lifecycle for bound devices.
//!
//! The reboot and power-off terminals in `khal::power` are bare endpoints:
//! they never quiesce devices. This module owns the driver-core side of an
//! orderly shutdown: it refuses new probe attempts, then walks every bound
//! device in reverse registration order and invokes the driver's
//! [`DeviceDriver::shutdown`] hook through the same serialized transition the
//! suspend path uses.
//!
//! # Probe synchronization
//!
//! A shutdown traversal must not race a binding attempt: a driver whose
//! `probe` just succeeded may have armed DMA or IRQs that its `shutdown` hook
//! is then required to quiesce. Synchronization is two-layered and needs no
//! new locks:
//!
//! 1. [`PROBE_LOCKDOWN`] refuses new binding claims. `probe_device_desc` and
//!    the driver-registration scan check the flag before claiming a
//!    descriptor, so a probe that has not started yet never runs during
//!    shutdown.
//! 2. The per-device transition takes the shared descriptor binding lock, so
//!    an *in-flight* probe or removal on a device the traversal visits
//!    completes first, and the lock recheck skips devices whose binding was
//!    replaced or retired while the traversal waited.
//!
//! # Ordering and error policy
//!
//! Devices are visited in reverse registration order ([`DeviceId`]
//! descending): registration roughly orders providers before their consumers,
//! so the reverse walk shuts consumers down first and a provider's DMA/IRQ
//! plumbing is still alive while its users quiesce. This mirrors the suspend
//! path, which sorts participants by topology depth for the same reason.
//!
//! Shutdown is best-effort by contract: a failing hook is logged and the walk
//! continues, because reboot must complete even when one device cannot stop.
//! No callback may abort the remaining traversal. The binding state is left
//! untouched: shutdown runs on the terminal path, and a state transition here
//! would only race the never-reached removal paths.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::{
    BindingRef, DeviceId, DeviceObject, DriverObject, find_driver, registry::device_registry,
};

static PROBE_LOCKDOWN: AtomicBool = AtomicBool::new(false);

/// Count of binding attempts currently executing, for tests and diagnostics.
static ACTIVE_PROBES: AtomicUsize = AtomicUsize::new(0);

/// Whether binding attempts are currently refused because the system is
/// shutting devices down.
pub fn probes_are_blocked() -> bool {
    PROBE_LOCKDOWN.load(Ordering::Acquire)
}

/// Reserves one binding claim.
///
/// Called by the probe paths immediately before a descriptor is claimed for
/// probing. Returns `false` when a shutdown traversal is in progress; the
/// caller must leave the descriptor untouched (`Pending`) and report the
/// refusal to its own caller. On `true` the caller owns a matching
/// [`finish_probe`] release.
pub(crate) fn try_begin_probe() -> bool {
    if PROBE_LOCKDOWN.load(Ordering::Acquire) {
        return false;
    }
    ACTIVE_PROBES.fetch_add(1, Ordering::AcqRel);
    true
}

/// Releases one binding claim reserved by [`try_begin_probe`].
pub(crate) fn finish_probe() {
    ACTIVE_PROBES.fetch_sub(1, Ordering::AcqRel);
}

/// Number of binding attempts currently executing.
pub fn active_probe_count() -> usize {
    ACTIVE_PROBES.load(Ordering::Acquire)
}

/// Blocks new binding attempts until the returned guard is dropped.
///
/// In-flight probes are deliberately not waited for here: the traversal below
/// serializes against them through each device's descriptor binding lock,
/// which is both sufficient and deadlock-free (a global wait would deadlock
/// against a probe that itself registers a driver and scans other
/// descriptors).
struct ProbeBlockGuard;

impl ProbeBlockGuard {
    fn acquire() -> Self {
        PROBE_LOCKDOWN.store(true, Ordering::Release);
        Self
    }
}

impl Drop for ProbeBlockGuard {
    fn drop(&mut self) {
        PROBE_LOCKDOWN.store(false, Ordering::Release);
    }
}

/// One bound device prepared for a shutdown transition.
struct ShutdownTarget {
    device: Arc<DeviceObject>,
    binding: BindingRef,
    driver: Arc<DriverObject>,
}

/// Snapshot the bound devices a traversal should visit.
///
/// Returns targets in reverse registration order. Devices without an active
/// binding are skipped: there is no driver hook to run for them. The binding
/// snapshot is revalidated under the binding lock by the transition.
fn shutdown_targets() -> Vec<ShutdownTarget> {
    let mut devices: Vec<(DeviceId, Arc<DeviceObject>)> = device_registry()
        .device_objects_snapshot()
        .into_iter()
        .map(|device| (device.id(), device))
        .collect();
    // Reverse registration order: consumers before providers.
    devices.sort_by_key(|entry| core::cmp::Reverse(entry.0));

    let mut targets = Vec::new();
    for (_, device) in devices {
        let Some(binding) = device.binding_ref() else {
            continue;
        };
        if !binding.is_active() {
            continue;
        }
        let Some(driver) = find_driver(binding.driver_id()) else {
            continue;
        };
        targets.push(ShutdownTarget {
            device,
            binding,
            driver,
        });
    }
    targets
}

/// Runs the system device-shutdown traversal.
///
/// Refuses new probe attempts for the duration, then invokes
/// [`DeviceDriver::shutdown`] for every currently bound device in reverse
/// registration order. Failures are logged and never abort the walk; the
/// caller (reboot/power-off) proceeds to the platform terminal regardless.
///
/// Must run in sleepable task context: descriptor binding locks may block.
/// Once the internal lockdown guard drops, probing resumes and a later
/// traversal sees the same bound set, so the function is safe to call again.
pub fn shutdown_all_devices() {
    let _lockdown = ProbeBlockGuard::acquire();
    let targets = shutdown_targets();
    log::info!(
        "device shutdown: visiting {} bound device(s)",
        targets.len()
    );
    for target in &targets {
        if let Err(error) = target
            .device
            .shutdown_transition(&target.binding, &target.driver)
        {
            log::warn!(
                "device shutdown: driver {} failed for device {:?}: {:?}; continuing",
                target.driver.name(),
                target.device.id(),
                error
            );
        }
    }
    log::info!("device shutdown: traversal complete");
}

unittest::path_mod_test!(tests, "../ut-tests/shutdown.rs");
