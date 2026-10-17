// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Root directory of a devpts mount: the `ptmx` master multiplexer.

use alloc::sync::Arc;

use devfs::DeviceFile;
use kvfs::{
    CharacterDeviceRegistration, Dentry, DeviceId, DirMaker, DirMapping, NodeFlags, SimpleDir,
    SimpleDirLookup, SimpleFs, VfsInode, VfsInodeInit, registered_character_devices,
};

/// `TTY_AUX_MAJOR`/`PTMX_MINOR`: the Unix98 ptmx device (Linux `devpts`).
const PTMX_DEVICE_ID: DeviceId = DeviceId::new(5, 2);

/// Name of the master multiplexer node mandated by the Unix98 pty layout.
const PTMX_NAME: &str = "ptmx";

pub(super) fn builder(fs: Arc<SimpleFs>) -> DirMaker {
    let mut root = DirMapping::new();
    if let Some(device) = ptmx_device(fs.clone()) {
        add_device_entry(&mut root, PTMX_NAME, device);
    } else {
        // devfs registers the shared ptmx multiplexer at boot; a devpts mount
        // without it has no ptmx backend to project.
        log::warn!("devpts: ptmx device {PTMX_DEVICE_ID:?} is not registered");
    }
    SimpleDir::new_maker(fs, Arc::new(root))
}

/// Projects the devfs-registered `ptmx` multiplexer into this filesystem.
///
/// The registration (and therefore the PTY driver) is owned by devfs; the
/// node only references it, so `open(2)` on either mount reaches the same
/// multiplexer.
fn ptmx_device(fs: Arc<SimpleFs>) -> Option<Arc<DeviceFile>> {
    registered_character_devices()
        .iter()
        .find(|registration| registration.device_id() == PTMX_DEVICE_ID)
        .map(|registration: &CharacterDeviceRegistration| {
            DeviceFile::from_registration(fs, registration)
        })
        .transpose()
        .ok()
        .flatten()
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
