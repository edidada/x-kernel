// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kvfs::{DirMapping, SimpleFs};

use crate::{DeviceFile, add_device_entry};

/// Adds device nodes for the resident whole disks registered by the block layer.
pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    for device in subsys_block::class::BLOCK.devices() {
        let number = device.with(|disk| disk.device_number());
        add_device_entry(
            root,
            device.name(),
            Ok(DeviceFile::new_block(fs.clone(), number)),
        );
    }
}
