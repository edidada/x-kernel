// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `net` class: its publication API (publish/devices/find/
//! subscribe), removal entries, and the class hooks — instantiated on
//! the shared machinery in `driver-base::class` (the Linux
//! drivers/net shape: the subsystem owns its whole class story).

use alloc::{boxed::Box, string::String, vec::Vec};

use driver_base::{
    ClassAvailabilityCallback, ClassDevice, ClassDeviceId, ClassParent, ClassRegistry,
    ClassRemovalCallback, ClassWithdraw,
    class::class_api::{ClassHooks, class_notify},
};
use kerrno::KResult;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use super::NetDevice;

pub type NetDeviceImpl = Box<dyn NetDevice + Send + Sync>;

static HOOKS: ClassHooks<NetDeviceImpl> = ClassHooks::passthrough();

driver_base::class_api!(
    "net",
    NetDeviceImpl,
    NET_DEVICES,
    net_class_registry,
    publish_net,
    net_devices,
    find_net_device,
    subscribe_net_available,
    subscribe_net_removed,
    HOOKS
);

/// Unpublish one `net` class device by publication id.
pub fn unpublish_net(id: ClassDeviceId) {
    driver_base::class::class_api::class_remove(net_class_registry, &HOOKS, id);
}

/// Guard form: withdraws this publication when the activation drops.
pub fn net_withdraw(device: &ClassDevice<NetDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_net, device.id())
}
