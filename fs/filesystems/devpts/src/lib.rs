// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Devpts pseudo-terminal filesystem.
//!
//! Each mount exposes a root directory holding the `ptmx` master
//! multiplexer, the shape runc's default OCI spec expects when it mounts
//! `devpts` with `newinstance` at `<rootfs>/dev/pts`. Opening `ptmx`
//! allocates a pty pair through the shared devfs PTY driver.
//!
//! Scope limits, acceptable until per-container pty instances are needed:
//! mount options (`newinstance`, `ptmxmode`, `mode`, `gid`) are accepted but
//! ignored, and slave pty nodes live in the shared devfs PTS table, so they
//! do not appear as entries of this filesystem.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

mod root;

use alloc::sync::Arc;

use kvfs::{
    FileSystemType, FsContext, FsContextOperations, SimpleFs, SuperBlock, SuperBlockFlags,
    VfsResult, get_tree_nodev,
};

/// `DEVPTS_SUPER_MAGIC` (Linux `uapi/linux/magic.h`).
const DEVPTS_SUPER_MAGIC: u32 = 0x0001_CD1C;

fn get_tree(
    context: &mut FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    get_tree_nodev(context, |file_system_type, flags| {
        Ok(new_devpts_with_type(file_system_type, flags))
    })
}

static FS_CONTEXT_OPERATIONS: FsContextOperations = FsContextOperations::new(get_tree);

fn init_fs_context(context: &mut FsContext<'_>) -> VfsResult<()> {
    context.set_operations(&FS_CONTEXT_OPERATIONS);
    Ok(())
}

/// Registered devpts filesystem type.
///
/// Registration happens in `fs_boot` (`prepare_namespace`) because nothing
/// else links this crate at boot; see the comment there.
pub static FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::nodev("devpts", init_fs_context);

fn new_devpts_with_type(
    file_system_type: &'static FileSystemType,
    flags: SuperBlockFlags,
) -> Arc<SuperBlock> {
    SimpleFs::new_with_superblock_flags(file_system_type, DEVPTS_SUPER_MAGIC, flags, root::builder)
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    #[def_test]
    fn filesystem_type_is_registered_as_devpts() {
        assert_eq!(super::FILE_SYSTEM_TYPE.name(), "devpts");
    }
}
