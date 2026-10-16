// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `virtio-9p` class instantiation: publication API and removal
//! entries over the family's own trait — the 9p "subsystem" lives with
//! the family (no standalone subsystem crate), so its class API does too.

use alloc::{string::String, vec::Vec};

use driver_base::{
    ClassAvailabilityCallback, ClassDevice, ClassDeviceId, ClassParent, ClassRegistry,
    ClassRemovalCallback, ClassWithdraw,
    class::class_api::{ClassHooks, class_notify},
};
use kerrno::KResult;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use crate::Virtio9pDevice;

pub type Virtio9pDeviceImpl = Box<dyn Virtio9pDevice + Send + Sync>;
use alloc::boxed::Box;

static HOOKS: ClassHooks<Virtio9pDeviceImpl> = ClassHooks::passthrough();

driver_base::class_api!(
    "virtio-9p",
    Virtio9pDeviceImpl,
    VIRTIO_9P_DEVICES,
    virtio_9p_class_registry,
    publish_virtio_9p,
    virtio_9p_devices,
    find_virtio_9p_device,
    subscribe_virtio_9p_available,
    subscribe_virtio_9p_removed,
    HOOKS
);

/// Unpublish one `virtio-9p` class device by publication id.
pub fn unpublish_virtio_9p(id: ClassDeviceId) {
    driver_base::class::class_api::class_remove(virtio_9p_class_registry, &HOOKS, id);
}

/// Guard form: withdraws this publication when the activation drops.
pub fn virtio_9p_withdraw(device: &ClassDevice<Virtio9pDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_virtio_9p, device.id())
}
