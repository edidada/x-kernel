// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 controls for the caller's network namespace.

use alloc::{borrow::Cow, format, sync::Arc};

use kprocess::AsThread;
use kvfs::{
    DirMapping, NodePermission, SimpleDir, SimpleFile, SimpleFileOps, SimpleFs, VfsError, VfsFile,
    VfsResult,
};

struct IpForwardFile;

impl SimpleFileOps for IpForwardFile {
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        let namespace = current_namespace()?;
        let value = knet::netns::ipv4_forwarding(&namespace)?;
        Ok(Cow::Owned(format!("{value}\n").into_bytes()))
    }

    fn write(&self, _file: &VfsFile, data: &[u8], offset: u64) -> VfsResult<usize> {
        let namespace = current_namespace()?;
        if !kprocess::current_cred().has_cap(kcred::CAP_NET_ADMIN) {
            return Err(VfsError::OperationNotPermitted);
        }
        if offset != 0 || data.is_empty() {
            return Ok(data.len());
        }
        let value = core::str::from_utf8(data)
            .map_err(|_| VfsError::InvalidInput)?
            .trim_ascii()
            .parse::<i32>()
            .map_err(|_| VfsError::InvalidInput)?;
        knet::netns::set_ipv4_forwarding(&namespace, value)?;
        Ok(data.len())
    }
}

fn current_namespace() -> VfsResult<Arc<kns::NetNamespace>> {
    let task = ktask::current();
    let thread = task.try_as_thread().ok_or(VfsError::NotFound)?;
    thread.process().net_ns()
}

pub(super) fn add_entries(sys: &mut DirMapping, fs: &Arc<SimpleFs>) {
    let mut ipv4 = DirMapping::new();
    ipv4.add(
        "ip_forward",
        SimpleFile::new_regular_with_permission(
            fs.clone(),
            NodePermission::OWNER_READ
                | NodePermission::OWNER_WRITE
                | NodePermission::GROUP_READ
                | NodePermission::OTHER_READ,
            IpForwardFile,
        ),
    );
    let mut net = DirMapping::new();
    net.add("ipv4", SimpleDir::new_maker(fs.clone(), Arc::new(ipv4)));
    sys.add("net", SimpleDir::new_maker(fs.clone(), Arc::new(net)));
}

/// Registers the built-in table enumeration used by unqualified iptables-save.
pub(super) fn add_root_entries(root: &mut DirMapping, fs: &Arc<SimpleFs>) {
    let mut net = DirMapping::new();
    net.add(
        "ip_tables_names",
        SimpleFile::new_regular_with_permission(
            fs.clone(),
            NodePermission::OWNER_READ | NodePermission::GROUP_READ,
            || {
                Ok(format!(
                    "{}\n",
                    knet::LegacyTableHandle::registered_names().join("\n")
                ))
            },
        ),
    );
    root.add("net", SimpleDir::new_maker(fs.clone(), Arc::new(net)));
}
