// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One device class as a single `const`-constructed static (the
//! mechanism ported from the parallel `driver-mvp` line, adapted to this
//! tree: the node projection stays with `devnode`, publications link to
//! the live [`crate::DeviceCore`]).
//!
//! A subsystem declares its class once:
//!
//! ```ignore
//! pub static NET: Class<NetDeviceImpl> = Class::new(());
//! pub static BLOCK: Class<BlockDeviceImpl, BlockHooks> = Class::new(BlockHooks);
//! ```
//!
//! `const fn new` builds the registry at link time — no lazy init, no
//! first-use branch. The whole publish / enumerate / subscribe /
//! withdraw / remove protocol lives in [`Class`] once, generic over the
//! payload `T` and the hooks `H` — no macro-generated API surface, no
//! call-site import requirements.

use alloc::{string::String, vec::Vec};

use devnode::NodeClass;
use kerrno::KResult;
use kspin::SpinNoPreempt;

use super::{ClassDevice, ClassDeviceId, ClassParent, ClassRegistry, ClassWithdraw};

/// Per-class behavior — the `struct class` function slots.
///
/// `()` covers the common hook-free class. A class with behavior —
/// block's `add_disk`/`del_gendisk`, display/input's devnode
/// projection — provides a small unit struct implementing this trait
/// and passes it to [`Class::new`]. The impl is on the subsystem's OWN
/// hook type, so the orphan rule is satisfied structurally. Hooks take
/// the payload by reference (`&T`): what lets block's `Arc<Gendisk>`
/// be cloned for `add_disk`.
pub trait ClassHooks<T>: Send + Sync + 'static {
    /// Class-side publish hook (`add_disk`, ...), run before registry
    /// insert; on registry failure [`unpublish`](Self::unpublish) unwinds
    /// it.
    fn publish(&self, _runtime: &T) -> KResult<()> {
        Ok(())
    }

    /// Class-side unpublish hook (`del_gendisk`, ...), run on removal.
    fn unpublish(&self, _runtime: &T) {}

    /// The devnode class this device projects on availability (input's
    /// eventN, DRM's cardN); `None` for classes without a projection.
    fn node_class(&self, _runtime: &T) -> Option<NodeClass> {
        None
    }
}

impl<T> ClassHooks<T> for () {}

/// One device class: a typed registry plus its hooks.
///
/// `static` requires `Class<T, H>: Sync`, which holds because the
/// registry is behind `SpinNoPreempt` and the payloads are
/// `Send + Sync`.
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
    /// The name arrives explicitly: the driver built it, it owns it. The
    /// class's publish hook runs before the registry insert; on registry
    /// failure the hook is unwound so no class-side state leaks.
    pub fn publish(
        &self,
        parent: impl Into<ClassParent>,
        runtime: T,
        name: impl Into<String>,
    ) -> KResult<ClassDevice<T>> {
        let name = name.into();
        let device = ClassDevice::try_new(parent, runtime, name)?;
        let id = device.id();
        let is_active = device.is_available();
        device.with(|runtime| self.hooks.publish(runtime))?;
        if let Err(error) = self.registry.lock().publish(device.clone()) {
            device.with(|runtime| self.hooks.unpublish(runtime));
            return Err(error);
        }
        log::debug!(
            "device class: published {id:?} (driver={})",
            device.driver_name()
        );
        if is_active {
            self.notify_available(id);
        }
        Ok(device)
    }

    /// Return every currently available device in this class.
    pub fn devices(&self) -> Vec<ClassDevice<T>> {
        self.registry.lock().devices()
    }

    /// Find one currently available device by its publication handle.
    pub fn find(&self, id: ClassDeviceId) -> Option<ClassDevice<T>> {
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

    /// The `Data`-owned withdrawal guard: dropping unpublishes.
    ///
    /// Takes `&'static self`: a class is a `static`, so the guard can
    /// close over it and outlive the caller (it rides the driver's
    /// `Data`).
    pub fn withdraw(&'static self, device: &ClassDevice<T>) -> ClassWithdraw {
        let id = device.id();
        ClassWithdraw::new(move || {
            self.unpublish(id);
        })
    }

    /// Full removal path by publication handle: close the usage gate, run
    /// removal callbacks, flip the device offline, withdraw the devnode
    /// projection, then run the class's unpublish hook.
    pub fn unpublish(&self, id: ClassDeviceId) {
        let (removed, callbacks) = {
            let mut registry = self.registry.lock();
            let removed = registry.remove(id);
            let callbacks = registry.removal_callbacks();
            (removed, callbacks)
        };
        if let Some(device) = removed {
            // Usage gate FIRST: new acquire_use calls fail from here; the
            // in-flight count at close time is what a draining subsystem
            // waits on. Then the driver's remove callbacks still see an
            // available device (their own final operations run under the
            // gate they already hold). Only then flip the shared offline
            // flag every consumer clone can observe, and withdraw.
            device.begin_drain();
            for callback in callbacks {
                callback(device.clone());
            }
            device.parent_ref().mark_offline();
            let node_id = device.parent_ref().id().raw();
            devnode::withdraw(node_id);
            device.with(|runtime| self.hooks.unpublish(runtime));
        }
    }

    /// Drain every publication owned by `owner` (the safety-net drain).
    pub fn drain_owner(&self, owner: u64) {
        loop {
            let Some(id) = self.registry.lock().find_detached(owner) else {
                break;
            };
            self.unpublish(id);
        }
    }

    /// Availability notification: project the class's devnode, then
    /// invoke subscribers outside the registry lock.
    fn notify_available(&self, id: ClassDeviceId) {
        let (device, callbacks) = {
            let registry = self.registry.lock();
            let Some(device) = registry.find(id) else {
                return;
            };
            (device, registry.availability_callbacks())
        };

        if let Some(node_class) = device.with(|runtime| self.hooks.node_class(runtime)) {
            let node_id = device.parent_ref().id().raw();
            let parent_path = String::from(device.location());
            let driver_name = String::from(device.driver_name());
            node_class.project(node_id, driver_name.as_str(), parent_path.as_str());
        }

        for callback in callbacks {
            callback(device.clone());
        }
    }
}
