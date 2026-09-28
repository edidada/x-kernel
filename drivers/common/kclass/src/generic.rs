// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic runtime device class primitives.
//!
//! This module owns the class-device handle and typed registry mechanics.
//! Concrete class adapters such as block, net, char, display, input, vsock,
//! and 9P are wired in the crate root.

use alloc::{string::String, sync::Arc, vec::Vec};

use devnode::DeviceLocation;
use driver_base::{Device, DeviceKind, DriverError, DriverResult};

/// Opaque handle of one class-device publication.
///
/// Allocated from a dedicated id space when a driver publishes. Consumers
/// treat it as an opaque capability handle (compare, store, pass back to
/// `find_*`/console APIs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClassDeviceId(u64);

/// High bit tags driver-core (new model) owned publications.
const DRIVER_CORE_ID_TAG: u64 = 1 << 63;

static NEXT_DETACHED_ID: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(DRIVER_CORE_ID_TAG + 1);

impl ClassDeviceId {
    /// Allocate a fresh driver-core owned id.
    pub fn new_detached() -> Self {
        Self(NEXT_DETACHED_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed))
    }

    /// Derive a record key from a devnode publication id (deterministic,
    /// collision-free with allocated ids: node ids never carry the tag bit
    /// used by allocated detached ids... actually they may; use a fresh
    /// derivation instead — hash the node id into the tagged space).
    pub fn new_detached_for_node(node_id: u64) -> Self {
        // Simple FNV-style fold into the tagged space; node ids and
        // allocated ids share the space but this derivation is injective
        // enough for record keys (id equality is what matters).
        let mut h = node_id ^ DRIVER_CORE_ID_TAG;
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        Self(h)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// The parent identity a class device is published under: an identity
/// snapshot (publication id, driver name, location) from the publishing
/// domain; availability is managed by the composition root's remove
/// notifier.
pub enum ClassParent {
    Detached(DetachedParent),
}

/// Value snapshot for driver-core publications.
pub struct DetachedParent {
    pub id: ClassDeviceId,
    /// The owning driver-core device id. Explicit, never derived from the
    /// publication id (Linux links class devices to their parent with a
    /// real pointer; id arithmetic silently decouples the two counters).
    pub owner: u64,
    pub driver_name: &'static str,
    pub location: DeviceLocation,
    /// Flipped when the composition root removes the owning device (after
    /// the quiesce callbacks returned). Shared by every [`ClassDevice`]
    /// clone through the inner `Arc`, so a consumer holding a stale clone
    /// can observe `is_available() == false` instead of silently poking a
    /// device the framework no longer stands behind.
    offline: core::sync::atomic::AtomicBool,
}

impl ClassParent {
    /// Publish under a driver-core device snapshot.
    ///
    /// `owner` is the publishing device's driver-core id (`DeviceCtx::id()`);
    /// the composition root's remove notifier uses it to find this
    /// publication when that device goes away.
    pub fn detached(owner: u64, driver_name: &'static str, location: DeviceLocation) -> Self {
        Self::Detached(DetachedParent {
            id: ClassDeviceId::new_detached(),
            owner,
            offline: core::sync::atomic::AtomicBool::new(false),
            driver_name,
            location,
        })
    }

    pub fn id(&self) -> ClassDeviceId {
        match self {
            Self::Detached(detached) => detached.id,
        }
    }

    pub fn driver_name(&self) -> Option<&'static str> {
        match self {
            Self::Detached(detached) => Some(detached.driver_name),
        }
    }

    pub fn location(&self) -> DeviceLocation {
        match self {
            Self::Detached(detached) => detached.location,
        }
    }

    fn is_available(&self) -> bool {
        match self {
            Self::Detached(detached) => {
                !detached.offline.load(core::sync::atomic::Ordering::Acquire)
            }
        }
    }

    /// Mark the parent device removed. Called by the removal orchestration
    /// after the quiesce callbacks have returned, before the devnode and
    /// class publication are withdrawn.
    pub fn mark_offline(&self) {
        match self {
            Self::Detached(detached) => detached
                .offline
                .store(true, core::sync::atomic::Ordering::Release),
        }
    }

    /// The owning device id, as passed to [`ClassParent::detached`].
    pub fn detached_owner(&self) -> Option<u64> {
        match self {
            Self::Detached(detached) => Some(detached.owner),
        }
    }
}

/// A typed runtime capability with a live parent device object.
pub struct ClassDevice<T> {
    inner: Arc<ClassDeviceInner<T>>,
}

struct ClassDeviceInner<T> {
    parent: ClassParent,
    runtime: T,
    name: String,
    device_kind: DeviceKind,
    irq: Option<usize>,
    metadata: ClassDeviceMetadata,
}

pub(crate) struct ClassDeviceMetadata {
    input_identity: Option<InputIdentity>,
}

struct InputIdentity {
    physical_location: String,
    unique_id: String,
}

impl ClassDeviceMetadata {
    pub(crate) const fn empty() -> Self {
        Self {
            input_identity: None,
        }
    }

    pub(crate) fn input(physical_location: String, unique_id: String) -> Self {
        Self {
            input_identity: Some(InputIdentity {
                physical_location,
                unique_id,
            }),
        }
    }
}

impl<T> Clone for ClassDevice<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> ClassDevice<T> {
    /// Create a class device with runtime-provided identity metadata.
    pub fn try_new_with_info(
        parent: impl Into<ClassParent>,
        inner: T,
        name: String,
        device_kind: DeviceKind,
        irq: Option<usize>,
    ) -> DriverResult<Self> {
        Self::try_new_with_class_metadata(
            parent,
            inner,
            name,
            device_kind,
            irq,
            ClassDeviceMetadata::empty(),
        )
    }

    pub(crate) fn try_new_with_class_metadata(
        parent: impl Into<ClassParent>,
        inner: T,
        name: String,
        device_kind: DeviceKind,
        irq: Option<usize>,
        metadata: ClassDeviceMetadata,
    ) -> DriverResult<Self> {
        let parent = parent.into();
        parent.driver_name().ok_or(DriverError::BadState)?;
        Ok(Self {
            inner: Arc::new(ClassDeviceInner {
                parent,
                runtime: inner,
                name,
                device_kind,
                irq,
                metadata,
            }),
        })
    }

    /// Unique class-publication handle (see [`ClassDeviceId`]).
    pub fn id(&self) -> ClassDeviceId {
        self.inner.parent.id()
    }

    /// The parent identity this device was published under.
    pub fn parent_ref(&self) -> &ClassParent {
        &self.inner.parent
    }

    /// Stable runtime device name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Runtime capability kind.
    pub fn device_kind(&self) -> DeviceKind {
        self.inner.device_kind
    }

    /// Optional IRQ associated with the runtime capability.
    pub fn irq(&self) -> Option<usize> {
        self.inner.irq
    }

    /// Where the device lives on the bus topology.
    pub fn location(&self) -> DeviceLocation {
        self.inner.parent.location()
    }

    /// Name of the driver that activated this device.
    /// Returns the bound driver's name.
    ///
    /// # Panics
    ///
    /// Panics if the parent device has no bound driver; publication
    /// through [`publish`](crate::prelude) validates this, so tripping
    /// the check indicates a bypass of the publish path.
    pub fn driver_name(&self) -> &'static str {
        self.inner
            .parent
            .driver_name()
            .expect("class device must have a bound driver")
    }

    /// Borrow the runtime capability.
    ///
    /// The runtime object is shared immutably; any mutation is the driver's
    /// responsibility via interior locking.
    ///
    /// This does **not** gate on [`Self::is_available`]: a clone taken
    /// before removal keeps calling into `T` after the parent device went
    /// away (the documented consumer-clone limitation). Consumers that can
    /// observe removal — subsystem dispatch layers, poll loops — should
    /// check [`Self::is_available`] first; drivers' own teardown paths run
    /// before the offline flag flips and may keep using `with`.
    pub fn with<'a, R>(&'a self, f: impl FnOnce(&'a T) -> R) -> R {
        f(&self.inner.runtime)
    }

    /// Whether the parent device is active and not removed.
    pub fn is_available(&self) -> bool {
        self.inner.parent.is_available()
    }

    /// Run a data-plane operation, refusing once the device is offline.
    ///
    /// Every consumer-facing class operation that returns a [`DriverResult`]
    /// goes through here: after the composition root removed the owning
    /// device (offline flips once the quiesce callbacks returned), the
    /// operation fails with [`DriverError::BadState`] instead of touching
    /// the possibly-torn-down runtime object. Internal teardown paths keep
    /// using [`Self::with`] — they run before the offline flag flips.
    pub fn with_available<R>(&self, f: impl FnOnce(&T) -> DriverResult<R>) -> DriverResult<R> {
        if !self.is_available() {
            return Err(DriverError::BadState);
        }
        f(&self.inner.runtime)
    }

    pub(crate) fn input_physical_location(&self) -> &str {
        self.inner
            .metadata
            .input_identity
            .as_ref()
            .map_or("", |identity| identity.physical_location.as_str())
    }

    pub(crate) fn input_unique_id(&self) -> &str {
        self.inner
            .metadata
            .input_identity
            .as_ref()
            .map_or("", |identity| identity.unique_id.as_str())
    }
}

impl<T> Device for ClassDevice<T>
where
    T: Send + Sync,
{
    fn name(&self) -> &str {
        self.name()
    }

    fn device_kind(&self) -> DeviceKind {
        self.device_kind()
    }

    fn irq(&self) -> Option<usize> {
        self.irq()
    }
}

impl<T> core::fmt::Debug for ClassDevice<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClassDevice")
            .field("id", &self.id())
            .field("location", &self.location())
            .field("driver_name", &self.driver_name())
            .finish()
    }
}

/// Notification callback invoked after a typed runtime capability is activated.
pub type ClassAvailabilityCallback<T> = Arc<dyn Fn(ClassDevice<T>) + Send + Sync>;

/// Callback invoked when a class device is removed.
pub type ClassRemovalCallback<T> = Arc<dyn Fn(ClassDevice<T>) + Send + Sync>;

/// Typed registry for one runtime device class.
pub struct ClassRegistry<T> {
    devices: Vec<ClassDevice<T>>,
    availability_subscribers: Vec<ClassAvailabilityCallback<T>>,
    removal_subscribers: Vec<ClassAvailabilityCallback<T>>,
}

impl<T> ClassRegistry<T> {
    /// Create an empty class registry.
    pub const fn new() -> Self {
        Self {
            devices: Vec::new(),
            availability_subscribers: Vec::new(),
            removal_subscribers: Vec::new(),
        }
    }

    /// Publish one runtime class device.
    ///
    /// Linux device registration rejects duplicate identities; replacement is
    /// a remove-then-add lifecycle rather than an implicit registry update.
    pub fn publish(&mut self, device: ClassDevice<T>) -> DriverResult<()> {
        let id = device.id();
        if self.devices.iter().any(|existing| existing.id() == id) {
            return Err(DriverError::AlreadyExists);
        }
        self.devices.push(device);
        Ok(())
    }

    /// Return currently active devices in this class.
    pub fn devices(&self) -> Vec<ClassDevice<T>> {
        self.devices
            .iter()
            .filter(|device| device.is_available())
            .cloned()
            .collect()
    }

    /// Find a currently active device in this class.
    pub fn find(&self, id: ClassDeviceId) -> Option<ClassDevice<T>> {
        self.devices
            .iter()
            .find(|device| device.id() == id && device.is_available())
            .cloned()
    }

    /// Subscribe to future availability notifications for this class.
    pub fn subscribe_available(&mut self, callback: ClassAvailabilityCallback<T>) {
        self.availability_subscribers.push(callback);
    }

    pub fn subscribe_removed(&mut self, callback: ClassAvailabilityCallback<T>) {
        self.removal_subscribers.push(callback);
    }

    pub fn removal_callbacks(&self) -> Vec<ClassAvailabilityCallback<T>> {
        self.removal_subscribers.clone()
    }

    /// Snapshot availability callbacks. Callers should invoke them outside locks.
    pub fn availability_callbacks(&self) -> Vec<ClassAvailabilityCallback<T>> {
        self.availability_subscribers.clone()
    }

    /// Remove a class device by its publication handle.
    pub fn remove(&mut self, id: ClassDeviceId) -> Option<ClassDevice<T>> {
        let pos = self.devices.iter().position(|device| device.id() == id)?;
        Some(self.devices.swap_remove(pos))
    }

    /// Find the publication id for a detached (driver-core owned) device.
    /// Legacy-model publications are never matched. Callers use this to run
    /// the FULL removal path (callbacks, devnode, unpublish) via `remove`.
    pub fn find_detached(&self, owner: u64) -> Option<crate::ClassDeviceId> {
        self.devices
            .iter()
            .find(|device| device.inner.parent.detached_owner() == Some(owner))
            .map(|device| device.id())
    }
}

impl<T> Default for ClassRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unittest)]
mod tests {
    //! Regression: the class publication ↔ driver-core owner association
    //! used to be derived by clearing a tag bit off an independently
    //! allocated publication id — an off-by-one that made remove(0) leak
    //! device 0's publication and remove(1) delete device 0's. The owner
    //! is explicit now; this pins the correlation.

    use unittest::{assert_eq, def_test};

    use super::*;

    fn loc(id: u32) -> DeviceLocation {
        DeviceLocation::FirmwareNode { id }
    }

    #[def_test(serial)]
    fn owner_correlation_is_explicit_not_derived() {
        // Two devices with domain ids 0 and 1 — exactly the values the
        // broken derivation shifted between.
        let a = ClassParent::detached(0, "test-a", loc(0));
        let b = ClassParent::detached(1, "test-b", loc(1));

        // The owner each publication reports is the one it was created
        // with, not id-derived arithmetic: the old code reported 1 and 2.
        assert_eq!(a.detached_owner(), Some(0));
        assert_eq!(b.detached_owner(), Some(1));
        // Distinct devices never collide on owner.
        assert!(a.detached_owner() != b.detached_owner());
    }

    #[def_test(serial)]
    fn publication_id_is_an_independent_concept() {
        // Publication ids are freshly allocated and need not correlate
        // with owner ids in any way — two parents with the same owner
        // shape still get distinct publication ids.
        let a = ClassParent::detached(7, "test", loc(0));
        let b = ClassParent::detached(7, "test", loc(1));
        let (id_a, id_b) = match (&a, &b) {
            (ClassParent::Detached(da), ClassParent::Detached(db)) => (da.id, db.id),
        };
        assert!(id_a != id_b);
        assert_eq!(a.detached_owner(), Some(7));
        assert_eq!(b.detached_owner(), Some(7));
    }
}
