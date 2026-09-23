// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic runtime device class primitives.
//!
//! This module owns the class-device handle and typed registry mechanics.
//! Concrete class adapters such as block, net, char, display, input, vsock,
//! and 9P are wired in the crate root.

use alloc::{string::String, sync::Arc, vec::Vec};

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kdevice::{DeviceId, DeviceLocation, DeviceObject, DeviceState};

/// Opaque handle of one class-device publication.
///
/// Its own id space: legacy-model publications derive it from the publishing
/// `DeviceObject` (same numeric value), while driver-core publications
/// allocate from a dedicated high range so the two models can never collide.
/// Consumers treat it as an opaque capability handle (compare, store, pass
/// back to `find_*`/console APIs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClassDeviceId(u64);

/// High bit tags driver-core (new model) owned publications.
const DRIVER_CORE_ID_TAG: u64 = 1 << 63;

static NEXT_DETACHED_ID: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(DRIVER_CORE_ID_TAG + 1);

impl ClassDeviceId {
    /// Derive from a legacy `kdevice` publication parent.
    pub const fn from_device(id: DeviceId) -> Self {
        Self(id.raw())
    }

    /// Allocate a fresh driver-core owned id.
    pub fn new_detached() -> Self {
        Self(NEXT_DETACHED_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed))
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// The parent identity a class device is published under.
///
/// Two origins coexist during the model migration:
/// * [`ClassParent::Device`] — legacy model: the live `DeviceObject` provides
///   availability state and removal events.
/// * [`ClassParent::Detached`] — driver-core model: a value snapshot (id,
///   driver name, location); availability is managed by the publishing
///   domain, removal arrives through its remove notifier.
pub enum ClassParent {
    Device(Arc<DeviceObject>),
    Detached(DetachedParent),
}

/// Value snapshot for driver-core publications.
pub struct DetachedParent {
    pub id: ClassDeviceId,
    pub driver_name: &'static str,
    pub location: DeviceLocation,
}

impl ClassParent {
    /// Publish under a driver-core device snapshot.
    pub fn detached(driver_name: &'static str, location: DeviceLocation) -> Self {
        Self::Detached(DetachedParent {
            id: ClassDeviceId::new_detached(),
            driver_name,
            location,
        })
    }

    pub fn id(&self) -> ClassDeviceId {
        match self {
            Self::Device(device) => ClassDeviceId::from_device(device.id()),
            Self::Detached(detached) => detached.id,
        }
    }

    pub fn driver_name(&self) -> Option<&'static str> {
        match self {
            Self::Device(device) => device.driver_name(),
            Self::Detached(detached) => Some(detached.driver_name),
        }
    }

    pub fn location(&self) -> DeviceLocation {
        match self {
            Self::Device(device) => device.location(),
            Self::Detached(detached) => detached.location,
        }
    }

    fn is_available(&self) -> bool {
        match self {
            Self::Device(device) => device.state() == DeviceState::Active,
            Self::Detached(_) => true,
        }
    }

    /// The owning driver-core device id, if this is a detached publication.
    pub fn detached_owner(&self) -> Option<u64> {
        match self {
            Self::Device(_) => None,
            Self::Detached(detached) => Some(detached.id.raw() & !DRIVER_CORE_ID_TAG),
        }
    }
}

impl From<Arc<DeviceObject>> for ClassParent {
    fn from(device: Arc<DeviceObject>) -> Self {
        Self::Device(device)
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
    pub fn with<'a, R>(&'a self, f: impl FnOnce(&'a T) -> R) -> R {
        f(&self.inner.runtime)
    }

    /// Whether the parent device is active and not removed.
    pub fn is_available(&self) -> bool {
        self.inner.parent.is_available()
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

/// Typed registry for one runtime device class.
pub struct ClassRegistry<T> {
    devices: Vec<ClassDevice<T>>,
    availability_subscribers: Vec<ClassAvailabilityCallback<T>>,
}

impl<T> ClassRegistry<T> {
    /// Create an empty class registry.
    pub const fn new() -> Self {
        Self {
            devices: Vec::new(),
            availability_subscribers: Vec::new(),
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

    /// Snapshot availability callbacks. Callers should invoke them outside locks.
    pub fn availability_callbacks(&self) -> Vec<ClassAvailabilityCallback<T>> {
        self.availability_subscribers.clone()
    }

    /// Remove a class device by its publication handle.
    pub fn remove(&mut self, id: ClassDeviceId) -> Option<ClassDevice<T>> {
        let pos = self.devices.iter().position(|device| device.id() == id)?;
        Some(self.devices.swap_remove(pos))
    }

    /// Remove a detached (driver-core owned) class device by its owning
    /// device id. Legacy-model publications are never matched.
    pub fn remove_detached(&mut self, owner: u64) -> Option<ClassDevice<T>> {
        let pos = self
            .devices
            .iter()
            .position(|device| device.inner.parent.detached_owner() == Some(owner))?;
        Some(self.devices.swap_remove(pos))
    }
}

impl<T> Default for ClassRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}
