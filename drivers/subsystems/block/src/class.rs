// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `block` class: one const-constructed static with the
//! `add_disk`/`del_gendisk` hooks (the Linux `struct class block`
//! analogue).

use alloc::sync::Arc;

use driver_base::{
    ClassDevice,
    class::class_api::{Class, ClassHooks},
};
use kerrno::KResult;

use super::Gendisk;

/// The block class payload: one shared disk.
pub type BlockDeviceImpl = Arc<Gendisk>;

/// A published disk handle.
pub type BlockClassDevice = ClassDevice<BlockDeviceImpl>;

pub struct BlockHooks;

impl ClassHooks<BlockDeviceImpl> for BlockHooks {
    fn publish(&self, disk: &BlockDeviceImpl) -> KResult<()> {
        super::add_disk(disk.clone()).map(drop)
    }

    fn unpublish(&self, disk: &BlockDeviceImpl) {
        super::del_gendisk(disk.device_number());
    }
}

/// The block device class.
pub static BLOCK: Class<BlockDeviceImpl, BlockHooks> = Class::new(BlockHooks);
