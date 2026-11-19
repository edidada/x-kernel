// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS permission request and DAC checks.

use kcred::{CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, Cred};
use kerrno::{KError, KResult};

use crate::{Metadata, NodePermission, NodeType, VfsInode};

bitflags::bitflags! {
    /// Requested access permissions for a filesystem node.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Permission: u16 {
        /// Execute a file or search a directory.
        const MAY_EXEC = 0x0001;
        /// Write to a file or modify a directory.
        const MAY_WRITE = 0x0002;
        /// Read from a file or directory.
        const MAY_READ = 0x0004;
        /// Append to a file.
        const MAY_APPEND = 0x0008;
        /// Check access permission.
        const MAY_ACCESS = 0x0010;
        /// Open a file.
        const MAY_OPEN = 0x0020;
        /// Change current directory to this directory.
        const MAY_CHDIR = 0x0040;
    }
}

/// Applies the generic DAC permission policy to an inode.
///
/// Filesystems that do not override [`crate::InodeOperations::permission`]
/// reach this implementation.
pub fn generic_permission(inode: &VfsInode, permission: Permission, cred: &Cred) -> KResult<()> {
    // Linux `capable_wrt_inode_uidgid()` checks the capability in the caller's
    // user namespace and additionally requires the inode's uid/gid to be
    // mapped there. Using the superblock's `s_user_ns` here would deny a
    // namespaced root that owns a mapped ID on a host-mounted filesystem.
    check_dac_permission_in(&inode.metadata(), permission, cred, cred.user_ns())
}

#[cfg(unittest)]
fn check_dac_permission(metadata: &Metadata, permission: Permission, cred: &Cred) -> KResult<()> {
    check_dac_permission_in(metadata, permission, cred, &kcred::initial_user_namespace())
}

fn check_dac_permission_in(
    metadata: &Metadata,
    permission: Permission,
    cred: &Cred,
    user_ns: &kcred::UserNamespace,
) -> KResult<()> {
    let permission = permission
        .intersection(Permission::MAY_READ | Permission::MAY_WRITE | Permission::MAY_EXEC);
    let mode = metadata.mode.permission();
    if permission.is_empty() || has_all_requested_bits(mode, permission) {
        return Ok(());
    }

    // A regular file without any execute bit remains non-executable even for
    // `CAP_DAC_OVERRIDE`/`CAP_DAC_READ_SEARCH`; Linux rejects that case before
    // considering either capability.
    if permission.contains(Permission::MAY_EXEC)
        && metadata.mode.node_type() != NodeType::Directory
        && !has_any_exec_bit(metadata)
    {
        return Err(KError::PermissionDenied);
    }

    // `capable_wrt_inode_uidgid()` also fails when the inode's IDs are not
    // mapped into the caller's namespace.
    let inode_ids_mapped = user_ns.map_uid_from_initial(metadata.uid).is_some()
        && user_ns.map_gid_from_initial(metadata.gid).is_some();

    if inode_ids_mapped && cred.has_cap_in(user_ns, CAP_DAC_OVERRIDE) {
        return Ok(());
    }

    if inode_ids_mapped && cred.has_cap_in(user_ns, CAP_DAC_READ_SEARCH) {
        let readable = Permission::MAY_READ
            | if metadata.mode.node_type() == NodeType::Directory {
                Permission::MAY_EXEC
            } else {
                Permission::empty()
            };
        if permission.difference(readable).is_empty() {
            return Ok(());
        }
    }

    let in_group = cred.fsgid() == metadata.gid
        || cred
            .supplementary_groups()
            .binary_search(&metadata.gid)
            .is_ok();
    let is_allowed = if cred.fsuid() == metadata.uid {
        owner_allows(mode, permission)
    } else if in_group {
        group_allows(mode, permission)
    } else {
        other_allows(mode, permission)
    };

    if !is_allowed {
        return Err(KError::PermissionDenied);
    }

    Ok(())
}

/// Returns the [`Permission`] implied by read and write open access.
pub fn open_access_to_permission(read: bool, write: bool) -> Permission {
    let mut permission = Permission::empty();
    if read {
        permission |= Permission::MAY_READ;
    }
    if write {
        permission |= Permission::MAY_WRITE;
    }
    permission
}

fn has_any_exec_bit(metadata: &Metadata) -> bool {
    metadata.mode.permission().intersects(
        NodePermission::OWNER_EXEC | NodePermission::GROUP_EXEC | NodePermission::OTHER_EXEC,
    )
}

fn has_all_requested_bits(mode: NodePermission, permission: Permission) -> bool {
    owner_allows(mode, permission)
        && group_allows(mode, permission)
        && other_allows(mode, permission)
}

fn owner_allows(mode: NodePermission, permission: Permission) -> bool {
    allows(
        mode,
        permission,
        NodePermission::OWNER_READ,
        NodePermission::OWNER_WRITE,
        NodePermission::OWNER_EXEC,
    )
}

fn group_allows(mode: NodePermission, permission: Permission) -> bool {
    allows(
        mode,
        permission,
        NodePermission::GROUP_READ,
        NodePermission::GROUP_WRITE,
        NodePermission::GROUP_EXEC,
    )
}

fn other_allows(mode: NodePermission, permission: Permission) -> bool {
    allows(
        mode,
        permission,
        NodePermission::OTHER_READ,
        NodePermission::OTHER_WRITE,
        NodePermission::OTHER_EXEC,
    )
}

fn allows(
    mode: NodePermission,
    permission: Permission,
    read_bit: NodePermission,
    write_bit: NodePermission,
    exec_bit: NodePermission,
) -> bool {
    (!permission.contains(Permission::MAY_READ) || mode.contains(read_bit))
        && (!permission.contains(Permission::MAY_WRITE) || mode.contains(write_bit))
        && (!permission.contains(Permission::MAY_EXEC) || mode.contains(exec_bit))
}

unittest::path_mod_test!(tests, "../ut-tests/permission.rs");
