// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The class-device object: the publication/registry/drain machinery every
//! runtime device class runs on (Linux `struct device`'s class-membership
//! half, one crate with the device model at last).
//!
//! Family instantiation — the per-class registries, publish entry points,
//! and removal hooks — lives in `device-class` (the family layer). This
//! module stays family-agnostic: `ClassDevice<T>` is generic over the
//! runtime capability type.
//!
//! The family hooks (publish/unpublish side effects, node projection)
//! live in [`class_api`] + [`ClassRuntimeMetadata`]; the families
//! instantiate them in their own subsystem crates.

use alloc::{string::String, sync::Arc, vec::Vec};

use kerrno::{KError, KResult};

/// Opaque handle of one class-device publication.
///
/// Allocated from a dedicated id space when a driver publishes. Consumers
/// treat it as an opaque capability handle (compare, store, pass back to
/// `find_*`/console APIs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClassDeviceId(u64);

/// Ownership of one class publication's WITHDRAWAL: dropping this guard
/// unpublishes the publication it was created from.
///
/// This is the single publication-owner contract: the driver's `Data`
/// carries the guard as its FIRST field, so the withdrawal runs at the
/// start of `Data` drop — while the hardware is still mapped (consumer
/// detach callbacks see a live device) and BEFORE the IRQ/MMIO guards in
/// later fields release. The composition root no longer drains class
/// registries by owner id at remove time; each publication goes away with
/// exactly the Data that created it.
///
/// Creating the guard from a [`ClassDevice`](crate::ClassDevice) does not
/// keep the runtime alive: the registry's Arc and any consumer clones do
/// that; this guard only names the registry entry to withdraw.
/// One counted device use (see [`ClassDevice::acquire_use`]): dropping
/// decrements the in-flight count.
pub struct ClassUse<T> {
    device: ClassDevice<T>,
}

impl<T> Drop for ClassUse<T> {
    fn drop(&mut self) {
        self.device
            .inner
            .uses
            .fetch_sub(1, core::sync::atomic::Ordering::AcqRel);
    }
}

impl<T> core::ops::Deref for ClassUse<T> {
    type Target = ClassDevice<T>;

    fn deref(&self) -> &Self::Target {
        &self.device
    }
}

pub struct ClassWithdraw {
    unpublish: fn(ClassDeviceId),
    id: ClassDeviceId,
}

impl ClassWithdraw {
    /// Build from the class-specific withdraw constructor generated per
    /// class (e.g. [`crate::net_withdraw`]); `unpublish` is that class's
    /// id-keyed removal entry.
    pub fn new(unpublish: fn(ClassDeviceId), id: ClassDeviceId) -> Self {
        Self { unpublish, id }
    }
}

impl Drop for ClassWithdraw {
    fn drop(&mut self) {
        (self.unpublish)(self.id);
    }
}

/// High bit tags driver-core (new model) owned publications.
const DRIVER_CORE_ID_TAG: u64 = 1 << 63;

static NEXT_DETACHED_ID: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(DRIVER_CORE_ID_TAG + 1);

impl ClassDeviceId {
    /// Allocate a fresh driver-core owned id.
    pub fn new_detached() -> Self {
        Self(NEXT_DETACHED_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed))
    }

    /// Wrap an existing raw id without re-deriving it.
    ///
    /// For lookups where the caller already holds the id's owner (e.g. the
    /// devfs projection, whose node ids ARE class-device raw ids): the
    /// value is preserved exactly — any re-derivation (hashing) could
    /// never match the registry entry it came from.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
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
    /// The parent's physical path (bus-vtable derived at publication).
    pub physical_path: String,
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
    pub fn detached(owner: u64, driver_name: &'static str, physical_path: String) -> Self {
        Self::Detached(DetachedParent {
            id: ClassDeviceId::new_detached(),
            owner,
            offline: core::sync::atomic::AtomicBool::new(false),
            driver_name,
            physical_path,
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

    pub fn location(&self) -> &str {
        match self {
            Self::Detached(detached) => detached.physical_path.as_str(),
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
    /// Device-usage protocol (Linux `device usage`/`try_acquire`
    /// analogue): in-flight operations count here, and `class_remove`
    /// flips `draining` BEFORE the removal callbacks run — new acquires
    /// fail from that point. Operations already past the gate complete
    /// against the still-live runtime (the mapping follows the object —
    /// see the virtio lease model); WAITING for the count to drain is a
    /// subsystem-level protocol (block's `del_gendisk` drain, net's
    /// quiesce), not a class-core one.
    uses: core::sync::atomic::AtomicUsize,
    draining: core::sync::atomic::AtomicBool,
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
    ) -> KResult<Self> {
        Self::try_new(parent, inner, name)
    }

    /// Create a class device: the runtime, the name the driver built,
    /// and the parent it publishes under.
    pub fn try_new(parent: impl Into<ClassParent>, inner: T, name: String) -> KResult<Self> {
        let parent = parent.into();
        parent.driver_name().ok_or(KError::BadState)?;
        Ok(Self {
            inner: Arc::new(ClassDeviceInner {
                uses: core::sync::atomic::AtomicUsize::new(0),
                draining: core::sync::atomic::AtomicBool::new(false),
                parent,
                runtime: inner,
                name,
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

    /// Where the device lives on the bus topology.
    pub fn location(&self) -> &str {
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
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: publication validates the retained \
                      parent has a bound driver"
        )]
        {
            self.inner
                .parent
                .driver_name()
                .expect("class device must have a bound driver")
        }
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
    /// Every consumer-facing class operation that returns a [`KResult`]
    /// goes through here: after the composition root removed the owning
    /// device (offline flips once the quiesce callbacks returned), the
    /// operation fails with [`KError::BadState`] instead of touching
    /// the possibly-torn-down runtime object. Internal teardown paths keep
    /// using [`Self::with`] — they run before the offline flag flips.
    pub fn with_available<R>(&self, f: impl FnOnce(&T) -> KResult<R>) -> KResult<R> {
        if !self.is_available() {
            return Err(KError::BadState);
        }
        let _use = self.acquire_use()?;
        f(&self.inner.runtime)
    }

    /// Acquire a device-usage guard: counts one in-flight operation,
    /// refused once the removal orchestration closed the gate
    /// (`class_remove` flips `draining` before its callbacks). The guard
    /// decrements on drop. Long-running consumers (workers holding the
    /// device across awaits) hold THIS instead of a bare clone so removal
    /// observability is real: `in_flight()` reports the count.
    pub fn acquire_use(&self) -> Result<ClassUse<T>, KError> {
        if !self.is_available()
            || self
                .inner
                .draining
                .load(core::sync::atomic::Ordering::Acquire)
        {
            return Err(KError::BadState);
        }
        self.inner
            .uses
            .fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        Ok(ClassUse {
            device: self.clone(),
        })
    }

    /// In-flight operation count (diagnostics, removal orchestration).
    pub fn in_flight(&self) -> usize {
        self.inner.uses.load(core::sync::atomic::Ordering::Acquire)
    }

    /// Close the usage gate: new `acquire_use` calls fail. Returns the
    /// in-flight count AT close time — operations already past the gate
    /// complete against the still-live runtime; subsystems that must WAIT
    /// implement their drain (block: `del_gendisk`).
    pub fn begin_drain(&self) -> usize {
        self.inner
            .draining
            .store(true, core::sync::atomic::Ordering::Release);
        self.in_flight()
    }
}

impl<T> core::fmt::Debug for ClassDevice<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClassDevice")
            .field("id", &self.id())
            .field("physical_path", &self.location())
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
    pub fn publish(&mut self, device: ClassDevice<T>) -> KResult<()> {
        let id = device.id();
        if self.devices.iter().any(|existing| existing.id() == id) {
            return Err(KError::AlreadyExists);
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

pub mod class_api;

#[cfg(test)]
mod tests;
