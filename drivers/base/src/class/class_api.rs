// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The family-facing half of the class machinery: the hooks every class
//! implements, the shared notify/remove engines, and the `class_api!`
//! macro a subsystem instantiates to get its publish/devices/find/
//! subscribe API (Linux's `struct class` + `class_create` shape, one
//! instantiation per subsystem).

use alloc::string::String;

use devnode::NodeClass;
use kerrno::KResult;
use kspin::SpinNoPreempt;

use super::{ClassDeviceId, ClassRegistry};

/// Per-class behavior — the `struct class` function-pointer slots,
/// literally: plain function pointers the owning subsystem passes to
/// its `class_api!` instantiation. No trait impls, so no orphan-rule
/// contortions: a subsystem owns its hooks by defining them as fns.
pub struct ClassHooks<T> {
    /// Class side effects at publication (e.g. block's `add_disk`).
    pub publish: fn(&T) -> KResult<()>,
    /// Class side effects at removal (e.g. block's `del_gendisk`).
    pub unpublish: fn(&T),
    /// The devnode class this device projects (input's eventN, DRM's
    /// cardN); `None` for classes without a node projection.
    pub node_class: fn(&T) -> Option<NodeClass>,
}

impl<T> ClassHooks<T> {
    /// The no-op hook set: classes without side effects or node
    /// projections.
    pub const fn passthrough() -> Self {
        Self {
            publish: |_| Ok(()),
            unpublish: |_| {},
            node_class: |_| None,
        }
    }
}

/// Shared notify-with-devnode logic (replaces the macro's generated body).
pub fn class_notify<T>(
    registry_fn: fn() -> &'static SpinNoPreempt<ClassRegistry<T>>,
    hooks: &'static ClassHooks<T>,
    id: ClassDeviceId,
) {
    let (device, callbacks) = {
        let registry = registry_fn().lock();
        let Some(device) = registry.find(id) else {
            return;
        };
        (device, registry.availability_callbacks())
    };

    if let Some(node_class) = device.with(hooks.node_class) {
        let node_id = device.parent_ref().id().raw();
        let parent_location = device.location();
        node_class.project(node_id, device.driver_name(), parent_location);
    }

    for callback in callbacks {
        callback(device.clone());
    }
}

/// Shared removal logic.
pub fn class_remove<T>(
    registry_fn: fn() -> &'static SpinNoPreempt<ClassRegistry<T>>,
    hooks: &'static ClassHooks<T>,
    id: ClassDeviceId,
) {
    let (removed, callbacks) = {
        let mut registry = registry_fn().lock();
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
        device.with(hooks.unpublish);
    }
}

#[macro_export]
macro_rules! class_api {
    (
        $feature:literal,
        $ty:ty,
        $registry:ident,
        $registry_fn:ident,
        $publish_fn:ident,
        $devices_fn:ident,
        $find_fn:ident,
        $subscribe_fn:ident,
        $removed_subscriber_fn:ident,
        $hooks:ident
    ) => {
        static $registry: LazyInit<SpinNoPreempt<ClassRegistry<$ty>>> = LazyInit::new();

        fn $registry_fn() -> &'static SpinNoPreempt<ClassRegistry<$ty>> {
            $registry.call_once(|| SpinNoPreempt::new(ClassRegistry::new()));
            $registry.get().expect("class registry call_once succeeded")
        }

        #[doc = concat!("Publish a runtime capability into the `", $feature, "` class.")]
        pub fn $publish_fn(
            parent: impl Into<ClassParent>,
            runtime: $ty,
            name: impl Into<String>,
        ) -> KResult<ClassDevice<$ty>> {
            // The name arrives explicitly: the driver built it, it owns it.
            // No runtime-tag extraction — the registry's type is the class.
            let name = name.into();
            let device = ClassDevice::try_new(parent, runtime, name)?;
            device.with($hooks.publish)?;
            let id = device.id();
            let is_active = device.is_available();
            if let Err(error) = $registry_fn().lock().publish(device.clone()) {
                device.with($hooks.unpublish);
                return Err(error);
            }
            log::debug!(
                "device class '{}': published {:?} (driver={})",
                $feature,
                id,
                device.driver_name(),
            );
            if is_active {
                class_notify::<$ty>($registry_fn, &$hooks, id);
            }
            Ok(device)
        }

        #[doc = concat!("Return every currently available `", $feature, "` device.")]
        pub fn $devices_fn() -> Vec<ClassDevice<$ty>> {
            $registry_fn().lock().devices()
        }

        pub fn $find_fn(id: ClassDeviceId) -> Option<ClassDevice<$ty>> {
            $registry_fn().lock().find(id)
        }

        pub fn $subscribe_fn(callback: ClassAvailabilityCallback<$ty>) {
            $registry_fn().lock().subscribe_available(callback);
        }

        pub fn $removed_subscriber_fn(callback: ClassRemovalCallback<$ty>) {
            $registry_fn().lock().subscribe_removed(callback);
        }
    };
}
