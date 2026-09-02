// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Enumeration context for bus backends.
//!
//! Bus backends use [`EnumerationContext`] while emitting discovery-stage
//! descriptors. Matching/probing is run after enumeration through the current
//! `kdevice` compatibility bridge.

use alloc::vec::Vec;

use driver_base::DriverResult;
use kdevice::{
    BusId, DeviceDesc, DeviceDescId, DeviceId, DeviceIdentity, DeviceLocation, DiscoveryOrigin,
    ProbeOutcome, ResourceSet, TransportInfo,
};

/// Context used while bus backends enumerate device descriptors.
///
/// The backend-facing API records descriptors only. Probing remains a separate
/// step so bus/firmware discovery does not directly manufacture runtime device
/// objects.
pub struct EnumerationContext {
    pending: Vec<DeviceDesc>,
    unclaimed: Vec<DeviceDesc>,
    registered_count: usize,
}

impl EnumerationContext {
    /// Create an empty enumeration context.
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            unclaimed: Vec::new(),
            registered_count: 0,
        }
    }

    /// Record an enumerated descriptor that remains unclaimed after probing.
    pub fn add_unclaimed(&mut self, desc: DeviceDesc) {
        log::debug!("unclaimed device descriptor: {:?}", desc);
        self.unclaimed.push(desc);
    }

    /// Add a newly enumerated descriptor to the shared device core.
    pub fn register_device(
        &mut self,
        bus_id: BusId,
        location: DeviceLocation,
        origin: DiscoveryOrigin,
        identity: DeviceIdentity,
        transport: Option<TransportInfo>,
        resources: ResourceSet,
    ) -> DriverResult<DeviceDescId> {
        self.register_device_with_parent(
            bus_id, None, location, origin, identity, transport, resources,
        )
    }

    /// Same as [`register_device`], but records a parent device the resulting
    /// runtime object will be attached under once it is published.
    #[allow(clippy::too_many_arguments)]
    pub fn register_device_with_parent(
        &mut self,
        bus_id: BusId,
        parent: Option<DeviceId>,
        location: DeviceLocation,
        origin: DiscoveryOrigin,
        identity: DeviceIdentity,
        transport: Option<TransportInfo>,
        resources: ResourceSet,
    ) -> DriverResult<DeviceDescId> {
        let desc = kdevice::device_desc_add_with_parent(
            bus_id, parent, location, origin, identity, transport, resources,
        )?;
        let id = desc.id();
        self.registered_count += 1;
        self.pending.push(desc);
        Ok(id)
    }

    /// Probe every descriptor registered during this enumeration pass.
    pub fn probe_pending(&mut self) -> usize {
        let mut pending = core::mem::take(&mut self.pending);
        let mut activated = 0usize;

        loop {
            let mut deferred = Vec::new();
            let mut made_progress = false;
            for desc in pending {
                match kdevice::probe_device_desc(desc.id()) {
                    Ok(ProbeOutcome::Activated) => {
                        activated += 1;
                        made_progress = true;
                    }
                    Ok(ProbeOutcome::Skipped) => {}
                    Ok(ProbeOutcome::Deferred) => deferred.push(desc),
                    Ok(ProbeOutcome::Requeue | ProbeOutcome::Unclaimed) => self.add_unclaimed(desc),
                    Err(err) => {
                        log::warn!(
                            "failed to probe descriptor {:?} ({:?} at {:?}): {:?}",
                            desc.id(),
                            desc.identity(),
                            desc.location(),
                            err
                        );
                        self.add_unclaimed(desc);
                    }
                }
            }

            if deferred.is_empty() {
                break;
            }
            if !made_progress {
                for desc in deferred {
                    self.add_unclaimed(desc);
                }
                break;
            }
            // A provider or another dependency became active in this round.
            // Retry only deferred descriptors; every further round must
            // activate at least one device, which bounds the loop by the
            // number of descriptors and avoids spinning on a missing provider.
            pending = deferred;
        }

        activated
    }

    /// Iterate over all enumerated descriptors that were not immediately terminal.
    pub fn unclaimed(&self) -> &[DeviceDesc] {
        &self.unclaimed
    }

    /// Number of descriptors emitted in this enumeration pass.
    pub fn registered_count(&self) -> usize {
        self.registered_count
    }

    /// Number of enumerated descriptors that were not immediately terminal.
    pub fn unclaimed_count(&self) -> usize {
        self.unclaimed.len()
    }
}

impl Default for EnumerationContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use driver_base::{DeviceKind, DriverError, DriverResult};
    use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject, PlatformIdentity};
    use unittest::{assert_eq, def_test};

    use super::*;

    fn reset_model() -> alloc::sync::Arc<kdevice::BusInstance> {
        let (_bus_type, bus) =
            kdevice::reset_and_setup_platform_bus_for_tests("enumeration-test-bus");
        bus
    }

    struct DeferredDriver;

    static DEPENDENCY_READY: AtomicBool = AtomicBool::new(false);
    static CONSUMER_PROBES: AtomicUsize = AtomicUsize::new(0);

    impl DeviceDriver for DeferredDriver {
        fn name(&self) -> &'static str {
            "deferred-test"
        }

        fn device_kind(&self) -> DeviceKind {
            DeviceKind::Bus
        }

        fn bus_types(&self) -> &'static [BusTypeId] {
            &[BusTypeId::PLATFORM]
        }

        fn matcher(&self) -> &dyn DeviceMatcher {
            static MATCHER: kdevice::CompatibleAliasMatcher =
                kdevice::CompatibleAliasMatcher("deferred-device");
            &MATCHER
        }

        fn probe_device(&self, _device: Arc<DeviceObject>) -> DriverResult<()> {
            CONSUMER_PROBES.fetch_add(1, Ordering::Relaxed);
            if DEPENDENCY_READY.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(DriverError::WouldBlock)
            }
        }
    }

    struct DependencyDriver;

    impl DeviceDriver for DependencyDriver {
        fn name(&self) -> &'static str {
            "dependency-test"
        }

        fn device_kind(&self) -> DeviceKind {
            DeviceKind::Bus
        }

        fn bus_types(&self) -> &'static [BusTypeId] {
            &[BusTypeId::PLATFORM]
        }

        fn matcher(&self) -> &dyn DeviceMatcher {
            static MATCHER: kdevice::CompatibleAliasMatcher =
                kdevice::CompatibleAliasMatcher("dependency-device");
            &MATCHER
        }

        fn probe_device(&self, _device: Arc<DeviceObject>) -> DriverResult<()> {
            DEPENDENCY_READY.store(true, Ordering::Release);
            Ok(())
        }
    }

    #[def_test(serial)]
    fn test_register_device_creates_descriptor_without_runtime_record() {
        let bus = reset_model();

        let mut context = EnumerationContext::new();
        let id = context
            .register_device(
                bus.id(),
                DeviceLocation::PlatformStatic { id: 1 },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some("test-device"),
                    firmware_id: None,
                }),
                None,
                ResourceSet::new(),
            )
            .unwrap();

        let desc = kdevice::find_device_desc(id).unwrap();
        assert_eq!(desc.id(), id);
        assert_eq!(context.registered_count(), 1);
        assert_eq!(kdevice::device_records_snapshot().len(), 0);
    }

    #[def_test(serial)]
    fn test_probe_pending_requeues_unmatched_descriptor() {
        let bus = reset_model();

        let mut context = EnumerationContext::new();
        context
            .register_device(
                bus.id(),
                DeviceLocation::PlatformStatic { id: 2 },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some("test-device"),
                    firmware_id: None,
                }),
                None,
                ResourceSet::new(),
            )
            .unwrap();

        assert_eq!(context.probe_pending(), 0);
        assert_eq!(context.unclaimed_count(), 1);
        assert_eq!(
            context.unclaimed()[0].identity(),
            DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("test-device"),
                firmware_id: None,
            })
        );
    }

    #[def_test(serial)]
    fn retryable_probe_keeps_same_driver_eligible() {
        let bus = reset_model();
        DEPENDENCY_READY.store(false, Ordering::Release);
        CONSUMER_PROBES.store(0, Ordering::Relaxed);
        kdevice::register_driver_object(Arc::new(DeferredDriver));

        let desc = kdevice::device_desc_add(
            bus.id(),
            DeviceLocation::PlatformStatic { id: 3 },
            DiscoveryOrigin::PlatformStatic,
            DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("deferred-device"),
                firmware_id: None,
            }),
            None,
            ResourceSet::new(),
        )
        .unwrap();

        assert_eq!(
            kdevice::probe_device_desc(desc.id()).unwrap(),
            ProbeOutcome::Deferred
        );
        assert_eq!(
            kdevice::probe_device_desc(desc.id()).unwrap(),
            ProbeOutcome::Deferred
        );
        assert_eq!(CONSUMER_PROBES.load(Ordering::Relaxed), 2);
    }

    #[def_test(serial)]
    fn probe_pending_retries_deferred_after_dependency_progress() {
        let bus = reset_model();
        DEPENDENCY_READY.store(false, Ordering::Release);
        CONSUMER_PROBES.store(0, Ordering::Relaxed);
        kdevice::register_driver_object(Arc::new(DeferredDriver));
        kdevice::register_driver_object(Arc::new(DependencyDriver));

        let mut context = EnumerationContext::new();
        for (id, alias) in [(4, "deferred-device"), (5, "dependency-device")] {
            context
                .register_device(
                    bus.id(),
                    DeviceLocation::PlatformStatic { id },
                    DiscoveryOrigin::PlatformStatic,
                    DeviceIdentity::Platform(PlatformIdentity {
                        alias: Some(alias),
                        firmware_id: None,
                    }),
                    None,
                    ResourceSet::new(),
                )
                .unwrap();
        }

        assert_eq!(context.probe_pending(), 2);
        assert_eq!(context.unclaimed_count(), 0);
        assert_eq!(CONSUMER_PROBES.load(Ordering::Relaxed), 2);
    }
}
