// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Enumeration context for bus backends.
//!
//! Bus backends use [`EnumerationContext`] while emitting discovery-stage
//! descriptors. Matching/probing runs after enumeration: staged model RNG
//! registrations first, then legacy `kdevice` descriptors. Counts and the
//! unclaimed list describe only the legacy path, not both frameworks.

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
    #[cfg(feature = "virtio-rng")]
    pub(crate) model_owner: alloc::sync::Arc<crate::platform_model::PlatformRngRegistration>,
    #[cfg(feature = "virtio-rng")]
    model_pending: Vec<crate::platform_model::StagedRng>,
    pending: Vec<DeviceDesc>,
    unclaimed: Vec<DeviceDesc>,
    registered_count: usize,
}

impl EnumerationContext {
    /// Create an empty enumeration context sharing the production platform owner.
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "virtio-rng")]
            model_owner: crate::platform_model::production_owner(),
            #[cfg(feature = "virtio-rng")]
            model_pending: Vec::new(),
            pending: Vec::new(),
            unclaimed: Vec::new(),
            registered_count: 0,
        }
    }

    #[cfg(feature = "virtio-rng")]
    pub(crate) fn stage_model_rng(
        &mut self,
        view: &khal::firmware::devices::FirmwareDeviceView<'_>,
    ) {
        match self.model_owner.stage(view) {
            Ok(Some(pending)) => self.model_pending.push(pending),
            Ok(None) => {}
            Err(error) => log::warn!("model RNG resource conversion rejected: {error:?}"),
        }
    }

    /// Record an enumerated descriptor that remains unclaimed after probing.
    pub fn add_unclaimed(&mut self, desc: DeviceDesc) {
        log::debug!("unclaimed device descriptor: {:?}", desc);
        self.unclaimed.push(desc);
    }

    /// Reuse an existing MMIO descriptor without accessing its owned registers.
    ///
    /// Platform discovery is serialized by the shared owner's discovery lock.
    /// Exact matches are queued
    /// for the core's existing probe/skip decision; overlapping descriptions are
    /// rejected rather than creating an alias of another transport's mapping.
    pub(crate) fn revisit_mmio(&mut self, base: usize, size: usize) -> bool {
        #[cfg(feature = "virtio-rng")]
        if self.model_owner.owns_range(base, size) {
            return true;
        }
        let existing = kdevice::device_descs_snapshot().into_iter().find(|desc| {
            let DeviceLocation::Mmio {
                base: start,
                size: len,
            } = desc.location()
            else {
                return false;
            };
            size != 0
                && len != 0
                && base < start.saturating_add(len)
                && start < base.saturating_add(size)
        });
        let Some(desc) = existing else {
            return false;
        };
        if desc.location() != (DeviceLocation::Mmio { base, size }) {
            log::warn!(
                "platform: skipping MMIO range overlapping descriptor {:?}",
                desc.id()
            );
        } else if !self.pending.iter().any(|pending| pending.id() == desc.id()) {
            self.pending.push(desc);
        }
        true
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
    #[expect(
        clippy::too_many_arguments,
        reason = "CG-024-lint-exceptions: clippy::too_many_arguments: Enumeration supplies the \
                  complete device descriptor and parent relationship in one registration"
    )]
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
    ///
    /// Requires sleepable task context with all bus-manager and platform
    /// discovery locks released. Model registration can initialize the root,
    /// install native services and synchronously invoke driver callbacks.
    /// Returns only the number of legacy descriptors activated by this call.
    /// Model registration success is not a binding-success count.
    pub fn probe_pending(&mut self) -> usize {
        #[cfg(feature = "virtio-rng")]
        for pending in core::mem::take(&mut self.model_pending) {
            if let Err(error) = pending.register() {
                log::warn!("model RNG registration failed without legacy fallback: {error:?}");
            }
        }
        let pending = core::mem::take(&mut self.pending);
        let mut activated = 0usize;

        for desc in pending {
            match kdevice::probe_device_desc(desc.id()) {
                Ok(ProbeOutcome::Activated) => activated += 1,
                Ok(ProbeOutcome::Skipped) => {}
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

        activated
    }

    /// Legacy descriptors requeued, unclaimed or failed during probing.
    /// Does not include model devices or distinguish those three outcomes.
    pub fn legacy_unclaimed(&self) -> &[DeviceDesc] {
        &self.unclaimed
    }

    /// Legacy descriptor registrations accepted in this enumeration pass.
    /// Excludes reused MMIO descriptors and all staged model registrations.
    pub fn legacy_registered_count(&self) -> usize {
        self.registered_count
    }

    /// Number of entries in the legacy requeued/unclaimed/failed list.
    /// This is not a count of devices with no matching driver across both models.
    pub fn legacy_unclaimed_count(&self) -> usize {
        self.unclaimed.len()
    }
}

impl Default for EnumerationContext {
    fn default() -> Self {
        Self::new()
    }
}

unittest::path_mod_test!(enumeration_unittest, "../ut-tests/enumeration_unittest.rs");
