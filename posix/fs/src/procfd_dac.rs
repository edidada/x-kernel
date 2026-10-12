// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DAC checks for live `/proc/<pid>/fd/<fd>` resolution (Linux `may_*` on target inodes).

use kcred::AccessIdKind;
use kerrno::KResult;
use kthread::with_current_credentials;
use kvfs::{Location, NodeType, Permission, check_permission, open_access_to_permission};
use linux_raw_sys::general::{O_PATH, O_RDONLY, O_WRONLY};

/// Maps `openat(2)` access mode flags to inode DAC bits.
pub(crate) fn dac_permission_for_open_flags(open_flags: u32) -> Permission {
    if open_flags & O_PATH != 0 {
        return Permission::MAY_OPEN;
    }
    open_access_to_permission(open_flags & 0b11 != O_WRONLY, open_flags & 0b11 != O_RDONLY)
}

/// Checks whether the caller may use the given inode reached via live procfd.
pub(crate) fn check_procfd_inode_dac(loc: &Location, mut permission: Permission) -> KResult<()> {
    let meta = loc.metadata()?;
    if meta.node_type == NodeType::Directory
        && (permission.is_empty()
            || permission.intersects(Permission::MAY_READ | Permission::MAY_WRITE))
    {
        permission |= Permission::MAY_EXEC;
    }
    with_current_credentials(|credentials| {
        let access = credentials.access_snapshot(AccessIdKind::Filesystem);
        check_permission(&meta, permission, &access)
    })
}

/// DAC for live procfd `openat` (delegates to [`check_procfd_inode_dac`]).
pub(crate) fn check_procfd_open_dac(loc: &Location, open_flags: u32) -> KResult<()> {
    check_procfd_inode_dac(loc, dac_permission_for_open_flags(open_flags))
}
