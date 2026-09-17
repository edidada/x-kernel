// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Root directory of a devpts mount: the `ptmx` master multiplexer.

use alloc::sync::Arc;

use devfs::{DeviceFile, Ptmx};
use kvfs::{
    Dentry, DeviceId, DirMaker, DirMapping, NodeFlags, SimpleDir, SimpleDirLookup, SimpleFs,
    VfsInode, VfsInodeInit,
};

/// `TTY_AUX_MAJOR`/`PTMX_MINOR`: the Unix98 ptmx device (Linux `devpts`).
const PTMX_DEVICE_ID: DeviceId = DeviceId::new(5, 2);

/// Name of the master multiplexer node mandated by the Unix98 pty layout.
const PTMX_NAME: &str = "ptmx";

pub fn builder(fs: Arc<SimpleFs>) -> DirMaker {
    let mut root = DirMapping::new();
    add_device_entry(
        &mut root,
        PTMX_NAME,
        DeviceFile::new_character(fs.clone(), PTMX_DEVICE_ID, Arc::new(Ptmx(fs.clone()))),
    );
    SimpleDir::new_maker(fs, Arc::new(root))
}

/// Mirrors devfs's entry projection: a directory child that materializes the
/// device inode on lookup.
fn add_device_entry(
    root: &mut DirMapping,
    name: impl Into<alloc::string::String>,
    device: Arc<DeviceFile>,
) {
    root.add_child(name, move |lookup, name| {
        Ok(device_dentry(lookup, name, device.clone()))
    });
}

fn device_dentry(lookup: SimpleDirLookup<'_>, name: &str, device: Arc<DeviceFile>) -> Dentry {
    let i_flags = NodeFlags::NON_CACHEABLE | device.flags();
    let init: VfsInodeInit = device.inode_init();
    let inode = VfsInode::new_special(device, i_flags, init);
    lookup.file_from_inode(name, inode)
}
