// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `vsock` class: its publication API (publish/devices/find/
//! subscribe), removal entries, and the class hooks — instantiated on
//! the shared machinery in `driver-base::class` (the Linux
//! drivers/virtio shape: the subsystem owns its whole class story).

use alloc::{boxed::Box, string::String, vec::Vec};

use driver_base::{
    ClassAvailabilityCallback, ClassDevice, ClassDeviceId, ClassParent, ClassRegistry,
    ClassRemovalCallback, ClassWithdraw,
    class::class_api::{ClassHooks, class_notify},
};
use kerrno::KResult;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use super::VsockDevice;

pub type VsockDeviceImpl = Box<dyn VsockDevice + Send + Sync>;

static HOOKS: ClassHooks<VsockDeviceImpl> = ClassHooks::passthrough();

driver_base::class_api!(
    "vsock",
    VsockDeviceImpl,
    VSOCK_DEVICES,
    vsock_class_registry,
    publish_vsock,
    vsock_devices,
    find_vsock_device,
    subscribe_vsock_available,
    subscribe_vsock_removed,
    HOOKS
);

/// Unpublish one `vsock` class device by publication id.
pub fn unpublish_vsock(id: ClassDeviceId) {
    driver_base::class::class_api::class_remove(vsock_class_registry, &HOOKS, id);
}

/// Guard form: withdraws this publication when the activation drops.
pub fn vsock_withdraw(device: &ClassDevice<VsockDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_vsock, device.id())
}
