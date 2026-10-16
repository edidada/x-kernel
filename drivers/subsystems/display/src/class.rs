// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `display` class: its publication API (publish/devices/find/
//! subscribe), removal entries, and the class hooks — instantiated on
//! the shared machinery in `driver-base::class` (the Linux
//! drivers/gpu/drm shape: the subsystem owns its whole class story).

use alloc::{boxed::Box, string::String, vec::Vec};

use devnode::NodeClass;
use driver_base::{
    ClassAvailabilityCallback, ClassDevice, ClassDeviceId, ClassParent, ClassRegistry,
    ClassRemovalCallback, ClassWithdraw,
    class::class_api::{ClassHooks, class_notify},
};
use kerrno::KResult;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use super::DisplayDevice;

pub type DisplayDeviceImpl = Box<dyn DisplayDevice + Send + Sync>;

fn node_class(_runtime: &DisplayDeviceImpl) -> Option<NodeClass> {
    Some(NodeClass::Drm)
}

static HOOKS: ClassHooks<DisplayDeviceImpl> = ClassHooks {
    publish: |_| Ok(()),
    unpublish: |_| {},
    node_class,
};

driver_base::class_api!(
    "display",
    DisplayDeviceImpl,
    DISPLAY_DEVICES,
    display_class_registry,
    publish_display,
    display_devices,
    find_display_device,
    subscribe_display_available,
    subscribe_display_removed,
    HOOKS
);

/// Unpublish one `display` class device by publication id.
pub fn unpublish_display(id: ClassDeviceId) {
    driver_base::class::class_api::class_remove(display_class_registry, &HOOKS, id);
}

/// Guard form: withdraws this publication when the activation drops.
pub fn display_withdraw(device: &ClassDevice<DisplayDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_display, device.id())
}
