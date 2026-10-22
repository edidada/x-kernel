// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One device class as a single `const`-constructed static (Linux
//! `struct class`).
//!
//! A subsystem declares its class once:
//!
//! ```ignore
//! pub static NET: Class<NetDeviceImpl> = Class::new(());
//! pub static BLOCK: Class<BlockDeviceImpl, BlockHooks> = Class::new(BlockHooks);
//! ```
//!
//! `const fn new` builds the registry at link time — no lazy init, no
//! first-use branch. The whole publish / enumerate / subscribe / withdraw /
//! drain protocol lives in [`Class`] once, generic over the payload `T` and
//! the hooks `H`. Drivers publish with ONE call, from the device handle:
//!
//! ```ignore
//! let publication = dev.publish(&CHAR, runtime, "serial")?;
//! ```
//!
//! The returned [`Registration`] owns registration; consumer [`Publication`]
//! handles do not withdraw. Final teardown waits for existing uses.

use alloc::{string::String, sync::Arc, vec::Vec};

use kerrno::{KError, KResult};
use kspin::SpinNoPreempt;

use crate::{
    Publication, Registration,
    owner::{LegacyOwner, PublicationSource},
};

/// Per-class behavior — the `struct class` function slots.
///
/// `()` covers the common hook-free class. A class with behavior — block's
/// `add_disk`/`del_gendisk` plus devnode projection, display/input's
/// cardN/eventN projection — provides a small unit struct implementing this
/// trait and passes it to [`Class::new`]. The impl is on the subsystem's OWN
/// hook type, so the orphan rule is satisfied structurally. Hooks take the
/// payload and read-only source by reference. Source queries are live, not a
/// publish-time snapshot. A projection requiring stable teardown keys must
/// retain those keys in its own state. Hooks run outside the registry lock.
pub trait ClassHooks<T>: Send + Sync + 'static {
    /// Class-side publish hook (`add_disk`, devnode projection, ...), run
    /// before registry insert; on registry failure [`unpublish`](Self::unpublish)
    /// unwinds it.
    fn publish(&self, _runtime: &T, _source: &dyn PublicationSource) -> KResult<()> {
        Ok(())
    }

    /// Class-side unpublish hook (`del_gendisk`, `devnode::withdraw`, ...),
    /// run on removal — after the last in-flight use when operations were
    /// still running.
    fn unpublish(&self, _runtime: &T, _source: &dyn PublicationSource) {}
}

impl<T> ClassHooks<T> for () {}

/// One device class: a typed registry plus its hooks.
///
/// `static` requires `Class<T, H>: Sync`, which holds because the registry is
/// behind `SpinNoPreempt` and the payloads are `Send + Sync`.
pub struct Class<T, H: ClassHooks<T> = ()> {
    registry: SpinNoPreempt<ClassRegistry<T>>,
    hooks: H,
}

impl<T, H: ClassHooks<T>> Class<T, H>
where
    T: Send + Sync + 'static,
{
    /// Const-construct the class registry (link-time, no lazy init).
    pub const fn new(hooks: H) -> Self {
        Self {
            registry: SpinNoPreempt::new(ClassRegistry::new()),
            hooks,
        }
    }

    /// Publish a runtime capability into this class.
    ///
    /// Called by `Device::publish` — the driver-side
    /// door — with the device's live identity. The name arrives explicitly:
    /// the driver built it, it owns it. The class's publish hook runs before
    /// the registry insert; on registry failure the hook is unwound so no
    /// class-side state leaks.
    pub fn publish(
        &'static self,
        owner: Arc<dyn PublicationSource>,
        runtime: T,
        name: impl Into<String>,
    ) -> KResult<Registration<T>> {
        // A publication without a claiming driver has no DEVPATH/uevent
        // identity to project; reject it at the boundary.
        if owner.driver_name().is_none() || !owner.is_online() {
            return Err(kerrno::KError::BadState);
        }
        let name = name.into();
        let id = self.registry.lock().allocate_id()?;
        let publication = Publication::new(owner, runtime, name, id);
        if let Err(error) =
            publication.with(|runtime| self.hooks.publish(runtime, publication.source()))
        {
            // Unwind the class-side publish hook; the publication itself was
            // never installed (no withdrawal to run).
            publication.with(|runtime| self.hooks.unpublish(runtime, publication.source()));
            return Err(error);
        }
        let is_active = publication.is_available();
        // Bind the registry result first: the 2024-edition `if let` keeps
        // scrutinee temporaries alive through the whole expression, and the
        // guard must NOT be held while `unpublish` (del_gendisk, devnode
        // withdraw — heavyweight, re-entrant) runs.
        let registered = self.registry.lock().publish(publication.clone());
        if let Err(error) = registered {
            publication.with(|runtime| self.hooks.unpublish(runtime, publication.source()));
            return Err(error);
        }
        log::debug!(
            "device class: published {:?} (driver={})",
            id,
            publication.driver_name().unwrap_or("<none>")
        );
        let registration = Registration::new(publication.clone(), move || self.withdraw(id));
        if is_active {
            self.notify_available(publication.clone());
        }
        Ok(registration)
    }

    /// Transitional door for a publisher still living on the legacy device
    /// model: publish `runtime` under a synthetic identity instead of a
    /// probed `Device`. `driver_name` and `path` stand in
    /// for the DEVPATH identity the legacy tree cannot supply.
    ///
    /// This exists only while the legacy virtio publisher migrates onto the
    /// framework; do not build new drivers on it. Hidden from the documented
    /// API surface on purpose — a migration shim must not become part of the
    /// long-term model (api-design.md).
    #[doc(hidden)]
    pub fn attach_legacy(
        &'static self,
        driver_name: &'static str,
        path: alloc::string::String,
        runtime: T,
        name: impl Into<String>,
    ) -> KResult<Registration<T>> {
        let owner = LegacyOwner::new(driver_name, path);
        self.publish(owner, runtime, name)
    }

    /// Return every currently available device in this class.
    pub fn devices(&self) -> Vec<Publication<T>> {
        self.registry.lock().devices()
    }

    /// Subscribe to future availability notifications for this class.
    pub fn subscribe_available(&self, callback: ClassAvailabilityCallback<T>) {
        self.registry.lock().subscribe_available(callback);
    }

    /// Subscribe to removal notifications for this class.
    pub fn subscribe_removed(&self, callback: ClassRemovalCallback<T>) {
        self.registry.lock().subscribe_removed(callback);
    }

    /// Withdraw one registration, close its gate and notify consumers.
    /// Source identity is unchanged. The unpublish hook waits for existing uses.
    fn withdraw(&'static self, id: u64) {
        let (removed, callbacks) = {
            let mut registry = self.registry.lock();
            let removed = registry.remove(id);
            let callbacks = registry.removal_callbacks();
            (removed, callbacks)
        };
        let Some(device) = removed else {
            return;
        };
        // Usage gate FIRST: new acquire_use calls fail from here; the
        // in-flight count at close time is what the class-side teardown
        // waits on. Then removal callbacks still see a live runtime (their
        // final operations run under the guard they already hold). Only then
        // withdraw this publication's projection.
        device.begin_drain();
        for callback in callbacks {
            callback(device.clone());
        }
        let in_flight = device.in_flight();
        if in_flight != 0 {
            log::debug!(
                "device class: {:?} withdrawn with {in_flight} operation(s) in flight; class \
                 teardown deferred to the last one",
                device.id()
            );
        }
        device.finish_or_defer(alloc::boxed::Box::new(move |runtime, source| {
            self.hooks.unpublish(runtime, source)
        }));
    }

    /// Availability notification: invoke subscribers outside the registry
    /// lock. Projection is a hook concern, not a class-core one.
    fn notify_available(&self, device: Publication<T>) {
        let callbacks = {
            let registry = self.registry.lock();
            if registry.find(device.id()).is_none() {
                return;
            }
            registry.availability_callbacks()
        };

        for callback in callbacks {
            callback(device.clone());
        }
    }
}

/// Notification callback invoked after a class device is published while its
/// owning device is available.
pub type ClassAvailabilityCallback<T> = Arc<dyn Fn(Publication<T>) + Send + Sync>;

/// Notification callback invoked when a class device is removed.
pub type ClassRemovalCallback<T> = Arc<dyn Fn(Publication<T>) + Send + Sync>;

/// Typed registry for one runtime device class.
///
/// Holds consumer references, not registration credentials. Entries exist
/// between successful publish and the Registration's drop.
struct ClassRegistry<T> {
    next_id: u64,
    devices: Vec<Publication<T>>,
    availability_subscribers: Vec<ClassAvailabilityCallback<T>>,
    removal_subscribers: Vec<ClassRemovalCallback<T>>,
}

impl<T> ClassRegistry<T> {
    const fn new() -> Self {
        Self {
            next_id: 1,
            devices: Vec::new(),
            availability_subscribers: Vec::new(),
            removal_subscribers: Vec::new(),
        }
    }

    fn allocate_id(&mut self) -> KResult<u64> {
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or(KError::BadState)?;
        Ok(id)
    }

    /// Publish one runtime class device.
    ///
    /// Linux device registration rejects duplicate identities; replacement is
    /// a remove-then-add lifecycle rather than an implicit registry update.
    /// Publication ids are minted fresh per publish, so the duplicate test is
    /// (owning device identity, runtime name) — the same device publishing
    /// the same name twice is a driver bug and is refused.
    fn publish(&mut self, device: Publication<T>) -> KResult<()> {
        let duplicate = self
            .devices
            .iter()
            .any(|current| current.shares_identity_with(&device));
        if duplicate {
            return Err(KError::AlreadyExists);
        }
        self.devices.push(device);
        Ok(())
    }

    /// Every available device in this class.
    fn devices(&self) -> Vec<Publication<T>> {
        self.devices
            .iter()
            .filter(|device| device.is_available())
            .cloned()
            .collect()
    }

    /// Find one device by its publication handle.
    fn find(&self, id: u64) -> Option<Publication<T>> {
        self.devices
            .iter()
            .find(|device| device.id() == id && device.is_available())
            .cloned()
    }

    fn remove(&mut self, id: u64) -> Option<Publication<T>> {
        let position = self.devices.iter().position(|device| device.id() == id)?;
        Some(self.devices.swap_remove(position))
    }

    fn subscribe_available(&mut self, callback: ClassAvailabilityCallback<T>) {
        self.availability_subscribers.push(callback);
    }

    fn subscribe_removed(&mut self, callback: ClassRemovalCallback<T>) {
        self.removal_subscribers.push(callback);
    }

    fn availability_callbacks(&self) -> Vec<ClassAvailabilityCallback<T>> {
        self.availability_subscribers.clone()
    }

    fn removal_callbacks(&self) -> Vec<ClassRemovalCallback<T>> {
        self.removal_subscribers.clone()
    }
}
