// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `block` class: one const-constructed static with the
//! `add_disk`/`del_gendisk` hooks (the Linux `struct class block`
//! analogue).

use alloc::{string::String, sync::Arc, vec::Vec};

use driver_base::class::class_api::{Class, ClassHooks, PublicationNode};
use kerrno::KResult;

use super::Gendisk;

/// The block class payload: one shared disk.
pub type BlockDeviceImpl = Arc<Gendisk>;

pub struct BlockHooks;

impl ClassHooks<BlockDeviceImpl> for BlockHooks {
    fn publish(&self, disk: &BlockDeviceImpl, node: &PublicationNode) -> KResult<()> {
        super::add_disk(disk.clone())?;
        // Project the disk into devtmpfs/sysfs/uevent. Node key: the
        // publication id (one key space for every class); the kernel's
        // `dev_t` is the node's device number, not its registry key.
        devnode::publish(
            node.id,
            "block",
            String::from(disk.name()),
            disk.device_number(),
            node.parent_path.as_str(),
            disk.devnode_modalias().cloned(),
            Vec::new(),
            Some(node.driver_name.clone()),
        );
        Ok(())
    }

    fn unpublish(&self, disk: &BlockDeviceImpl, node: &PublicationNode) {
        devnode::withdraw(node.id);
        super::del_gendisk(disk.device_number());
    }
}

/// The block device class.
pub static BLOCK: Class<BlockDeviceImpl, BlockHooks> = Class::new(BlockHooks);
