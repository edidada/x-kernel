// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic runtime device class primitives.
//!
//! This module owns the class-device handle and typed registry mechanics.
//! Concrete class adapters such as block, net, char, display, input, vsock,
//! and 9P are wired in the crate root.

use alloc::{string::String, sync::Arc, vec::Vec};

use driver_base::{DriverError, DriverResult};

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
    pub(crate) fn new(unpublish: fn(ClassDeviceId), id: ClassDeviceId) -> Self {
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
    pub physical_path: alloc::string::String,
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
    pub fn detached(
        owner: u64,
        driver_name: &'static str,
        physical_path: alloc::string::String,
    ) -> Self {
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
    irq: Option<usize>,
    metadata: ClassDeviceMetadata,
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

pub struct ClassDeviceMetadata {
    input_identity: Option<InputIdentity>,
}

struct InputIdentity {
    physical_location: String,
    unique_id: String,
}

impl ClassDeviceMetadata {
    pub const fn empty() -> Self {
        Self {
            input_identity: None,
        }
    }

    pub fn input(physical_location: String, unique_id: String) -> Self {
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
        irq: Option<usize>,
    ) -> DriverResult<Self> {
        Self::try_new_with_class_metadata(parent, inner, name, irq, ClassDeviceMetadata::empty())
    }

    pub fn try_new_with_class_metadata(
        parent: impl Into<ClassParent>,
        inner: T,
        name: String,
        irq: Option<usize>,
        metadata: ClassDeviceMetadata,
    ) -> DriverResult<Self> {
        let parent = parent.into();
        parent.driver_name().ok_or(DriverError::BadState)?;
        Ok(Self {
            inner: Arc::new(ClassDeviceInner {
                uses: core::sync::atomic::AtomicUsize::new(0),
                draining: core::sync::atomic::AtomicBool::new(false),
                parent,
                runtime: inner,
                name,
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

    /// Optional IRQ associated with the runtime capability.
    pub fn irq(&self) -> Option<usize> {
        self.inner.irq
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
        let _use = self.acquire_use()?;
        f(&self.inner.runtime)
    }

    /// Acquire a device-usage guard: counts one in-flight operation,
    /// refused once the removal orchestration closed the gate
    /// (`class_remove` flips `draining` before its callbacks). The guard
    /// decrements on drop. Long-running consumers (workers holding the
    /// device across awaits) hold THIS instead of a bare clone so removal
    /// observability is real: `in_flight()` reports the count.
    pub fn acquire_use(&self) -> Result<ClassUse<T>, DriverError> {
        if !self.is_available()
            || self
                .inner
                .draining
                .load(core::sync::atomic::Ordering::Acquire)
        {
            return Err(DriverError::BadState);
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

    pub fn input_physical_location(&self) -> &str {
        self.inner
            .metadata
            .input_identity
            .as_ref()
            .map_or("", |identity| identity.physical_location.as_str())
    }

    pub fn input_unique_id(&self) -> &str {
        self.inner
            .metadata
            .input_identity
            .as_ref()
            .map_or("", |identity| identity.unique_id.as_str())
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
pub(crate) type ClassRemovalCallback<T> = Arc<dyn Fn(ClassDevice<T>) + Send + Sync>;

/// Typed registry for one runtime device class.
pub(crate) struct ClassRegistry<T> {
    devices: Vec<ClassDevice<T>>,
    availability_subscribers: Vec<ClassAvailabilityCallback<T>>,
    removal_subscribers: Vec<ClassAvailabilityCallback<T>>,
}

impl<T> ClassRegistry<T> {
    /// Create an empty class registry.
    pub(crate) const fn new() -> Self {
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
    pub(crate) fn publish(&mut self, device: ClassDevice<T>) -> DriverResult<()> {
        let id = device.id();
        if self.devices.iter().any(|existing| existing.id() == id) {
            return Err(DriverError::AlreadyExists);
        }
        self.devices.push(device);
        Ok(())
    }

    /// Return currently active devices in this class.
    pub(crate) fn devices(&self) -> Vec<ClassDevice<T>> {
        self.devices
            .iter()
            .filter(|device| device.is_available())
            .cloned()
            .collect()
    }

    /// Find a currently active device in this class.
    pub(crate) fn find(&self, id: ClassDeviceId) -> Option<ClassDevice<T>> {
        self.devices
            .iter()
            .find(|device| device.id() == id && device.is_available())
            .cloned()
    }

    /// Subscribe to future availability notifications for this class.
    pub(crate) fn subscribe_available(&mut self, callback: ClassAvailabilityCallback<T>) {
        self.availability_subscribers.push(callback);
    }

    pub(crate) fn subscribe_removed(&mut self, callback: ClassAvailabilityCallback<T>) {
        self.removal_subscribers.push(callback);
    }

    pub(crate) fn removal_callbacks(&self) -> Vec<ClassAvailabilityCallback<T>> {
        self.removal_subscribers.clone()
    }

    /// Snapshot availability callbacks. Callers should invoke them outside locks.
    pub(crate) fn availability_callbacks(&self) -> Vec<ClassAvailabilityCallback<T>> {
        self.availability_subscribers.clone()
    }

    /// Remove a class device by its publication handle.
    pub(crate) fn remove(&mut self, id: ClassDeviceId) -> Option<ClassDevice<T>> {
        let pos = self.devices.iter().position(|device| device.id() == id)?;
        Some(self.devices.swap_remove(pos))
    }

    /// Find the publication id for a detached (driver-core owned) device.
    /// Legacy-model publications are never matched. Callers use this to run
    /// the FULL removal path (callbacks, devnode, unpublish) via `remove`.
    pub(crate) fn find_detached(&self, owner: u64) -> Option<crate::ClassDeviceId> {
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

    fn loc(id: u32) -> alloc::string::String {
        alloc::format!("platform/firmware{id}")
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

    /// Regression: `remove_all_detached` has a hand-written per-class drain
    /// branch; a new class added to `class_api!` gets publish/unpublish
    /// generated but its remove branch must be added by hand — hwrng landed
    /// without one and its publications leaked on device removal. This test
    /// publishes a sentinel to the classes without composition-root
    /// side effects (hwrng — the actual regression — and net) and asserts
    /// the aggregate drain empties them. char/display/input are excluded:
    /// their publish/remove paths project devnode publications and switch
    /// the console, which needs the full composition root — those run
    /// under boot-time removal instead. Forgetting a branch still turns
    /// this red through hwrng/net.
    // split-6: the drain test needs hwrng or net publishers; both features
    // arrive with the virtio family in split 7 (char/display/input are
    // excluded by design — see the doc comment above).
    #[cfg(any(feature = "hwrng", feature = "net"))]
    #[def_test(serial)]
    fn remove_all_detached_drains_every_enabled_class() {
        use crate::remove_all_detached;

        #[cfg_attr(
            not(any(feature = "hwrng", feature = "net")),
            expect(
                dead_code,
                reason = "split-6: sentinel publishers arrive with the hwrng/net features in \
                          split 7"
            )
        )]
        fn loc(id: u16) -> alloc::string::String {
            alloc::format!("platform/static{id}")
        }

        let owner: u64 = 0xfeed_beef;
        let mut published = 0usize;

        #[cfg(feature = "hwrng")]
        {
            struct SentinelHwrng;
            impl hwrng::Device for SentinelHwrng {
                fn name(&self) -> &str {
                    "drain-sentinel"
                }
            }
            impl hwrng::HwrngDevice for SentinelHwrng {
                fn read(&self, _buf: &mut [u8]) -> driver_base::DriverResult<usize> {
                    Ok(0)
                }

                fn is_failed(&self) -> bool {
                    false
                }
            }
            crate::publish_hwrng(
                ClassParent::detached(owner, "drain-test", loc(1)),
                alloc::sync::Arc::new(SentinelHwrng),
            )
            .unwrap();
            published += 1;
        }

        #[cfg(feature = "net")]
        {
            struct SentinelNet;
            impl driver_base::Device for SentinelNet {
                fn name(&self) -> &str {
                    "net-sentinel"
                }
            }
            impl net::NetDevice for SentinelNet {
                fn mac(&self) -> net::MacAddress {
                    net::MacAddress([0; 6])
                }

                fn can_tx(&self) -> bool {
                    false
                }

                fn can_rx(&self) -> bool {
                    false
                }

                fn rx_queue_len(&self) -> usize {
                    0
                }

                fn tx_queue_len(&self) -> usize {
                    0
                }

                fn recycle_rx(&self, _rx_buf: net::NetBufHandle) -> driver_base::DriverResult {
                    Ok(())
                }

                fn recycle_tx(&self) -> driver_base::DriverResult {
                    Ok(())
                }

                fn send(&self, _tx_buf: net::NetBufHandle) -> driver_base::DriverResult {
                    Err(driver_base::DriverError::Unsupported)
                }

                fn recv(&self) -> driver_base::DriverResult<net::NetBufHandle> {
                    Err(driver_base::DriverError::Unsupported)
                }

                fn alloc_tx_buf(
                    &self,
                    _size: usize,
                ) -> driver_base::DriverResult<net::NetBufHandle> {
                    Err(driver_base::DriverError::Unsupported)
                }
            }
            crate::publish_net(
                ClassParent::detached(owner, "drain-test", loc(3)),
                alloc::boxed::Box::new(SentinelNet),
            )
            .unwrap();
            published += 1;
        }

        assert!(published > 0, "no class features enabled in this build");
        remove_all_detached(owner);

        macro_rules! assert_drained {
            ($feature:literal, $registry:path) => {
                #[cfg(feature = $feature)]
                assert!($registry().lock().find_detached(owner).is_none());
            };
        }
        assert_drained!("hwrng", crate::hwrng_class_registry);
        assert_drained!("net", crate::net_class_registry);
    }

    #[def_test(serial)]
    fn same_owner_multiple_publications_all_removable() {
        // Regression: remove_all_detached used to take only the FIRST
        // registry hit per class — a device publishing two class devices of
        // one class leaked the second. The registry drain itself is what
        // remove_all_detached loops on; pin it here.
        use crate::class::ClassRegistry;

        fn loc(id: u32) -> alloc::string::String {
            alloc::format!("platform/firmware{id}")
        }

        let mut registry: ClassRegistry<u32> = ClassRegistry::default();
        let a = ClassDevice::try_new_with_info(
            ClassParent::detached(9, "test", loc(0)),
            1u32,
            alloc::string::String::from("a"),
            None,
        )
        .unwrap();
        let b = ClassDevice::try_new_with_info(
            ClassParent::detached(9, "test", loc(1)),
            2u32,
            alloc::string::String::from("b"),
            None,
        )
        .unwrap();
        registry.publish(a).unwrap();
        registry.publish(b).unwrap();

        // Drain every publication of owner 9 — the second hit must be
        // reachable after the first removal (the old single-find bug).
        let mut drained = 0;
        while let Some(id) = registry.find_detached(9) {
            registry.remove(id);
            drained += 1;
        }
        assert_eq!(drained, 2);
        assert!(registry.find_detached(9).is_none());
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
    /// The MMIO lease model, pinned end to end at the class layer: the
    /// register mapping lives in the RUNTIME object, so unpublishing (the
    /// activation's close — driver Data teardown) must not release it while
    /// a consumer clone is still out. Linux parity: `pci_release_regions`
    /// runs after the LAST `put_device`, not at driver unbind; and once the
    /// removal orchestration flips the offline flag, the data plane is
    /// refused (`netif_tx_disable`) while observation still resolves valid
    /// memory (no dangling window).
    #[cfg(feature = "net")]
    #[def_test(serial)]
    fn unpublish_keeps_consumer_clone_leases_alive() {
        use core::sync::atomic::{AtomicBool, Ordering};

        use crate::NetDevice;

        struct LeaseOnDrop(Arc<AtomicBool>);
        impl Drop for LeaseOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        struct SentinelNet {
            _lease: LeaseOnDrop,
        }
        impl driver_base::Device for SentinelNet {
            fn name(&self) -> &str {
                "lease-sentinel"
            }
        }
        impl NetDevice for SentinelNet {
            fn mac(&self) -> net::MacAddress {
                net::MacAddress([0; 6])
            }

            fn can_tx(&self) -> bool {
                false
            }

            fn can_rx(&self) -> bool {
                false
            }

            fn rx_queue_len(&self) -> usize {
                0
            }

            fn tx_queue_len(&self) -> usize {
                0
            }

            fn recycle_rx(&self, _rx_buf: net::NetBufHandle) -> driver_base::DriverResult {
                Ok(())
            }

            fn recycle_tx(&self) -> driver_base::DriverResult {
                Ok(())
            }

            fn send(&self, _tx_buf: net::NetBufHandle) -> driver_base::DriverResult {
                Err(driver_base::DriverError::Unsupported)
            }

            fn recv(&self) -> driver_base::DriverResult<net::NetBufHandle> {
                Err(driver_base::DriverError::Unsupported)
            }

            fn alloc_tx_buf(&self, _size: usize) -> driver_base::DriverResult<net::NetBufHandle> {
                Err(driver_base::DriverError::Unsupported)
            }

            fn set_rx_scheduler(
                &self,
                _scheduler: Option<Arc<dyn net::NetRxScheduler>>,
            ) -> driver_base::DriverResult {
                Err(driver_base::DriverError::Unsupported)
            }
        }

        let released = Arc::new(AtomicBool::new(false));
        let runtime: crate::NetDeviceImpl = alloc::boxed::Box::new(SentinelNet {
            _lease: LeaseOnDrop(released.clone()),
        });
        let publication = crate::publish_net(
            ClassParent::detached(0x1eaf, "lease-test", loc(0x1eaf)),
            runtime,
        )
        .unwrap();

        // A consumer holds a clone across the device's removal — the exact
        // window the old Data-scoped mapping made a dangling pointer.
        let consumer = publication.clone();

        // The activation's close: unpublish by publication id.
        crate::unpublish_net(publication.id());
        drop(publication);

        // The mapping is NOT released with the publication (nor with the
        // driver Data): the clone's runtime still owns it.
        assert!(!released.load(Ordering::Acquire));

        // Data-plane admission is closed — netif_tx_disable parity: the
        // poll loop sees a stopped queue instead of retrying errors.
        assert!(!consumer.can_tx());
        assert!(!consumer.can_rx());
        // Observation still resolves valid memory (the runtime is alive);
        // nothing dangles.
        let _mac = consumer.mac();

        // The LAST reference drops — only now does the mapping release
        // (pci_release_regions after the last put_device).
        drop(consumer);
        assert!(released.load(Ordering::Acquire));
    }
}
