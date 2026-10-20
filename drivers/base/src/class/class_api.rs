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
//! The returned [`Publication`] is the owner: dropping it unpublishes (after
//! the last in-flight use), and it carries the class-side teardown.

use alloc::{string::String, sync::Arc, vec::Vec};

use kerrno::KResult;
use kspin::SpinNoPreempt;

use super::{ClassRegistry, Publication, PublicationId};
use crate::device::DeviceCore;

/// The identity a class hook needs to publish its user-visible projection
/// (`devnode::publish`, `add_disk`, ...).
///
/// Owned, because the teardown hook runs from the publication's deferred
/// finalizer and outlives the borrow of the device.
pub struct PublicationNode {
    /// The publication handle — the node registry's key.
    pub id: PublicationId,
    /// The claiming driver's name (DEVPATH/uevent metadata).
    pub driver_name: String,
    /// The device's physical path (DEVPATH/sysfs parent).
    pub parent_path: String,
}

/// Per-class behavior — the `struct class` function slots.
///
/// `()` covers the common hook-free class. A class with behavior — block's
/// `add_disk`/`del_gendisk` plus devnode projection, display/input's
/// cardN/eventN projection — provides a small unit struct implementing this
/// trait and passes it to [`Class::new`]. The impl is on the subsystem's OWN
/// hook type, so the orphan rule is satisfied structurally. Hooks take the
/// payload by reference (`&T`) plus the publication identity.
pub trait ClassHooks<T>: Send + Sync + 'static {
    /// Class-side publish hook (`add_disk`, devnode projection, ...), run
    /// before registry insert; on registry failure [`unpublish`](Self::unpublish)
    /// unwinds it.
    fn publish(&self, _runtime: &T, _node: &PublicationNode) -> KResult<()> {
        Ok(())
    }

    /// Class-side unpublish hook (`del_gendisk`, `devnode::withdraw`, ...),
    /// run on removal — after the last in-flight use when operations were
    /// still running.
    fn unpublish(&self, _runtime: &T, _node: &PublicationNode) {}
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
    /// Called by [`Device::publish`](crate::Device::publish) — the driver-side
    /// door — with the device's live identity. The name arrives explicitly:
    /// the driver built it, it owns it. The class's publish hook runs before
    /// the registry insert; on registry failure the hook is unwound so no
    /// class-side state leaks.
    pub(crate) fn publish(
        &'static self,
        owner: Arc<DeviceCore>,
        runtime: T,
        name: impl Into<String>,
    ) -> KResult<Publication<T>> {
        // A publication without a claiming driver has no DEVPATH/uevent
        // identity to project; reject it at the boundary.
        if owner.driver_name().is_none() {
            return Err(kerrno::KError::BadState);
        }
        let name = name.into();
        let id = PublicationId::next();
        let publication = Publication::new(owner, runtime, name, id);
        let node = PublicationNode {
            id,
            driver_name: String::from(publication.driver_name().unwrap_or("")),
            parent_path: String::from(publication.location()),
        };
        if let Err(error) = publication.with(|runtime| self.hooks.publish(runtime, &node)) {
            // Unwind the class-side publish hook; the publication itself was
            // never installed (no withdrawal to run).
            publication.with(|runtime| self.hooks.unpublish(runtime, &node));
            return Err(error);
        }
        publication.install_withdraw(move || self.withdraw(id));
        let is_active = publication.is_available();
        // Bind the registry result first: the 2024-edition `if let` keeps
        // scrutinee temporaries alive through the whole expression, and the
        // guard must NOT be held while `unpublish` (del_gendisk, devnode
        // withdraw — heavyweight, re-entrant) runs.
        let registered = self.registry.lock().publish(publication.clone());
        if let Err(error) = registered {
            publication.with(|runtime| self.hooks.unpublish(runtime, &node));
            return Err(error);
        }
        log::debug!(
            "device class: published {:?} (driver={})",
            id,
            publication.driver_name().unwrap_or("<none>")
        );
        if is_active {
            self.notify_available(publication.clone());
        }
        Ok(publication)
    }

    /// Transitional door for a publisher still living on the legacy device
    /// model: publish `runtime` under a synthetic identity instead of a
    /// probed [`Device`](crate::Device). `driver_name` and `path` stand in
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
    ) -> KResult<Publication<T>> {
        let owner = DeviceCore::synthetic(crate::DeviceId(0), driver_name, path);
        self.publish(owner, runtime, name)
    }

    /// Return every currently available device in this class.
    pub fn devices(&self) -> Vec<Publication<T>> {
        self.registry.lock().devices()
    }

    /// Find one currently available device by its publication handle.
    pub fn find(&self, id: PublicationId) -> Option<Publication<T>> {
        self.registry.lock().find(id)
    }

    /// Subscribe to future availability notifications for this class.
    pub fn subscribe_available(&self, callback: super::ClassAvailabilityCallback<T>) {
        self.registry.lock().subscribe_available(callback);
    }

    /// Subscribe to removal notifications for this class.
    pub fn subscribe_removed(&self, callback: super::ClassRemovalCallback<T>) {
        self.registry.lock().subscribe_removed(callback);
    }

    /// The withdrawal path, run exactly once by the owning publication's drop:
    /// remove from the registry, close the usage gate, run removal callbacks,
    /// flip the device offline, withdraw the devnode projection, then run the
    /// class's unpublish hook (deferred to the last in-flight use when one is
    /// still running).
    fn withdraw(&'static self, id: PublicationId) {
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
        // flip the shared offline flag every consumer clone observes, and
        // withdraw the projection.
        device.begin_drain();
        for callback in callbacks {
            callback(device.clone());
        }
        device.mark_offline();
        let in_flight = device.in_flight();
        if in_flight != 0 {
            log::debug!(
                "device class: {:?} withdrawn with {in_flight} operation(s) in flight; class \
                 teardown deferred to the last one",
                device.id()
            );
        }
        let node = PublicationNode {
            id: device.id(),
            driver_name: String::from(device.driver_name().unwrap_or("")),
            parent_path: String::from(device.location()),
        };
        device.finish_or_defer(alloc::boxed::Box::new(move |runtime| {
            self.hooks.unpublish(runtime, &node)
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
