// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `block` class: its publication API (publish/devices/find/
//! subscribe), removal entries, and the class hooks — instantiated on
//! the shared machinery in `driver-base::class` (the Linux
//! drivers/block shape: the subsystem owns its whole class story).

use alloc::{string::String, sync::Arc, vec::Vec};

use driver_base::{
    ClassAvailabilityCallback, ClassDevice, ClassDeviceId, ClassParent, ClassRegistry,
    ClassRemovalCallback, ClassWithdraw,
    class::class_api::{ClassHooks, class_notify},
};
use kerrno::KResult;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use super::Gendisk;

pub type BlockDeviceImpl = Arc<Gendisk>;

fn publish(disk: &BlockDeviceImpl) -> KResult<()> {
    super::add_disk(disk.clone()).map(drop)
}

fn unpublish(disk: &BlockDeviceImpl) {
    super::del_gendisk(disk.device_number());
}

static HOOKS: ClassHooks<BlockDeviceImpl> = ClassHooks {
    publish,
    unpublish,
    node_class: |_| None,
};

driver_base::class_api!(
    "block",
    BlockDeviceImpl,
    BLOCK_DEVICES,
    block_class_registry,
    publish_block,
    block_devices,
    find_block_device,
    subscribe_block_available,
    subscribe_block_removed,
    HOOKS
);

/// Unpublish one `block` class device by publication id.
pub fn unpublish_block(id: ClassDeviceId) {
    driver_base::class::class_api::class_remove(block_class_registry, &HOOKS, id);
}

/// Guard form: withdraws this publication when the activation drops.
pub fn block_withdraw(device: &ClassDevice<BlockDeviceImpl>) -> ClassWithdraw {
    ClassWithdraw::new(unpublish_block, device.id())
}
