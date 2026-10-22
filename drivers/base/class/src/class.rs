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
//! let publication = dev.publish(&CHAR, runtime)?;
//! ```
//!
//! The returned [`Registration`] owns registration; consumer [`Publication`]
//! handles do not withdraw. Final teardown waits for existing uses.

use alloc::{sync::Arc, vec::Vec};

use kerrno::KResult;
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
    /// before registry insert. If this hook fails, [`unpublish`](Self::unpublish)
    /// unwinds its partial effects before the error is returned.
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
    state: SpinNoPreempt<ClassState<T>>,
    hooks: H,
}

impl<T, H: ClassHooks<T>> Class<T, H>
where
    T: Send + Sync + 'static,
{
    /// Const-construct the class registry (link-time, no lazy init).
    pub const fn new(hooks: H) -> Self {
        Self {
            state: SpinNoPreempt::new(ClassState::new()),
            hooks,
        }
    }

    /// Publish a runtime capability into this class.
    ///
    /// Called by `Device::publish` — the driver-side
    /// door — with the device's live identity. Each successful call creates
    /// an independent registration, even for the same source and runtime.
    /// Subsystems own business uniqueness rules; a failing publish hook is
    /// unwound before returning its error.
    pub fn publish(
        &'static self,
        owner: Arc<dyn PublicationSource>,
        runtime: T,
    ) -> KResult<Registration<T>> {
        // A publication without a claiming driver has no DEVPATH/uevent
        // identity to project; reject it at the boundary.
        if owner.driver_name().is_none() || !owner.is_online() {
            return Err(kerrno::KError::BadState);
        }
        let publication = Publication::new(owner, runtime);
        if let Err(error) =
            publication.with(|runtime| self.hooks.publish(runtime, publication.source()))
        {
            // Unwind the class-side publish hook; the publication itself was
            // never installed (no withdrawal to run).
            publication.with(|runtime| self.hooks.unpublish(runtime, publication.source()));
            return Err(error);
        }
        let is_active = publication.is_available();
        self.state.lock().devices.push(publication.clone());
        log::debug!(
            "device class: published {:?} (driver={})",
            publication.location(),
            publication.driver_name().unwrap_or("<none>")
        );
        let registration = Registration::new(publication.clone(), move |publication| {
            self.withdraw(publication)
        });
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
    ) -> KResult<Registration<T>> {
        let owner = LegacyOwner::new(driver_name, path);
        self.publish(owner, runtime)
    }

    /// Return every currently available device in this class.
    pub fn devices(&self) -> Vec<Publication<T>> {
        self.state
            .lock()
            .devices
            .iter()
            .filter(|device| device.is_available())
            .cloned()
            .collect()
    }

    /// Subscribe to future availability notifications for this class.
    pub fn subscribe_available(&self, callback: ClassAvailabilityCallback<T>) {
        self.state.lock().availability_subscribers.push(callback);
    }

    /// Subscribe to removal notifications for this class.
    pub fn subscribe_removed(&self, callback: ClassRemovalCallback<T>) {
        self.state.lock().removal_subscribers.push(callback);
    }

    /// Withdraw one registration, close its gate and notify consumers.
    /// Source identity is unchanged. The unpublish hook waits for existing uses.
    fn withdraw(&'static self, publication: &Publication<T>) {
        let (removed, callbacks) = {
            let mut state = self.state.lock();
            let removed = state.remove(publication);
            let callbacks = state.removal_subscribers.clone();
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
                device.location()
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
            let state = self.state.lock();
            if !state.contains_available(&device) {
                return;
            }
            state.availability_subscribers.clone()
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

/// Shared state protected by one class lock.
///
/// Holds consumer references, not registration credentials. Entries exist
/// between successful publish and the Registration's drop.
struct ClassState<T> {
    devices: Vec<Publication<T>>,
    availability_subscribers: Vec<ClassAvailabilityCallback<T>>,
    removal_subscribers: Vec<ClassRemovalCallback<T>>,
}

impl<T> ClassState<T> {
    const fn new() -> Self {
        Self {
            devices: Vec::new(),
            availability_subscribers: Vec::new(),
            removal_subscribers: Vec::new(),
        }
    }

    fn contains_available(&self, publication: &Publication<T>) -> bool {
        self.devices
            .iter()
            .any(|device| device.same_publication(publication) && device.is_available())
    }

    fn remove(&mut self, publication: &Publication<T>) -> Option<Publication<T>> {
        let position = self
            .devices
            .iter()
            .position(|device| device.same_publication(publication))?;
        Some(self.devices.swap_remove(position))
    }
}
