// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `char` class: its publication API (publish/devices/find/
//! subscribe), removal entries, and the class hooks — instantiated on
//! the shared machinery in `driver-base::class` (the Linux
//! drivers/char shape: the subsystem owns its whole class story).

use alloc::{boxed::Box, string::String, vec::Vec};

use driver_base::{
    ClassAvailabilityCallback, ClassDevice, ClassDeviceId, ClassParent, ClassRegistry,
    ClassRemovalCallback, ClassWithdraw,
    class::class_api::{ClassHooks, class_notify},
};
use kerrno::KResult;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use super::CharDevice;

pub type CharDeviceImpl = Box<dyn CharDevice + Send + Sync>;

static HOOKS: ClassHooks<CharDeviceImpl> = ClassHooks::passthrough();

driver_base::class_api!(
    "char",
    CharDeviceImpl,
    CHAR_DEVICES,
    char_class_registry,
    publish_char,
    char_devices,
    find_char_device,
    subscribe_char_available,
    subscribe_char_removed,
    HOOKS
);

/// Unpublish one `char` class device by publication id.
pub fn unpublish_char(id: ClassDeviceId) {
    driver_base::class::class_api::class_remove(char_class_registry, &HOOKS, id);
}

/// Guard form: withdraws this publication when the activation drops.
pub fn char_withdraw(device: &ClassDevice<CharDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_char, device.id())
}
