// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-namespace projection files: `uid_map`, `gid_map`, and `setgroups`.

use alloc::{borrow::Cow, format, string::String, vec::Vec};

use kcred::{CAP_SETGID, CAP_SYS_ADMIN, IdMapExtent, UserNamespace};
use kprocess::AsThread;
use ktask::WeakKtaskRef;
use kvfs::{SimpleFileOps, VfsError, VfsFile, VfsResult};
use memaddr::PAGE_SIZE_4K;

use super::root::upgrade_task;

/// Which ID map a [`IdMapFile`] projects.
#[derive(Clone, Copy)]
pub(super) enum IdMapKind {
    Uid,
    Gid,
}

impl IdMapKind {
    fn map(self, ns: &UserNamespace) -> Vec<IdMapExtent> {
        match self {
            Self::Uid => ns.uid_map(),
            Self::Gid => ns.gid_map(),
        }
    }

    fn is_written(self, ns: &UserNamespace) -> bool {
        match self {
            Self::Uid => ns.has_uid_map(),
            Self::Gid => ns.has_gid_map(),
        }
    }

    fn write(
        self,
        ns: &UserNamespace,
        writer: &kcred::Cred,
        extents: Vec<IdMapExtent>,
    ) -> VfsResult<()> {
        match self {
            Self::Uid => ns.write_uid_map(writer, extents),
            Self::Gid => ns.write_gid_map(writer, extents),
        }
    }
}

/// `/proc/<pid>/{uid_map,gid_map}`.
pub(super) struct IdMapFile {
    task: WeakKtaskRef,
    kind: IdMapKind,
}

impl IdMapFile {
    pub(super) fn new(task: WeakKtaskRef, kind: IdMapKind) -> Self {
        Self { task, kind }
    }
}

impl SimpleFileOps for IdMapFile {
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        let task = upgrade_task(&self.task)?;
        let ns = task.as_thread().real_cred().user_ns_arc().clone();
        let mut text = String::new();
        if ns.parent().is_none() {
            // The initial namespace has an implicit identity map. Rendering the
            // full range keeps `/proc/1/uid_map` equal to Linux even though no
            // extents are stored.
            text.push_str(&format!("{:10} {:10} {:10}\n", 0, 0, u32::MAX));
        } else {
            let reader_ns = kprocess::current_cred().user_ns_arc().clone();
            let parent = ns.parent().ok_or(VfsError::NotFound)?;
            for extent in self.kind.map(&ns) {
                // `outside` is stored in parent-namespace coordinates. Linux
                // renders it in the reader's namespace, not the parent's.
                let global = match self.kind {
                    IdMapKind::Uid => parent.map_uid_to_initial(extent.outside()),
                    IdMapKind::Gid => parent.map_gid_to_initial(extent.outside()),
                };
                let outside = global
                    .and_then(|id| match self.kind {
                        IdMapKind::Uid => reader_ns.map_uid_from_initial(id),
                        IdMapKind::Gid => reader_ns.map_gid_from_initial(id),
                    })
                    .unwrap_or(65_534);
                text.push_str(&format!(
                    "{:10} {:10} {:10}\n",
                    extent.inside(),
                    outside,
                    extent.count()
                ));
            }
        }
        Ok(Cow::Owned(text.into_bytes()))
    }

    fn write(&self, file: &VfsFile, data: &[u8], offset: u64) -> VfsResult<usize> {
        let task = upgrade_task(&self.task)?;
        let target = task.as_thread().real_cred().user_ns_arc().clone();
        let writer = file.cred().clone();

        let Some(parent) = target.parent() else {
            return Err(VfsError::OperationNotPermitted);
        };
        if !core::ptr::eq(writer.user_ns(), &*target) && !core::ptr::eq(writer.user_ns(), &**parent)
        {
            return Err(VfsError::OperationNotPermitted);
        }
        if offset != 0 || data.len() >= PAGE_SIZE_4K {
            return Err(VfsError::InvalidInput);
        }
        if self.kind.is_written(&target) {
            return Err(VfsError::OperationNotPermitted);
        }
        if !writer.has_cap_in(&target, CAP_SYS_ADMIN) {
            return Err(VfsError::OperationNotPermitted);
        }

        let extents = parse_id_map(data)?;
        self.kind.write(&target, &writer, extents)?;
        Ok(data.len())
    }
}

/// `/proc/<pid>/setgroups`.
pub(super) struct SetgroupsFile {
    task: WeakKtaskRef,
}

impl SetgroupsFile {
    pub(super) fn new(task: WeakKtaskRef) -> Self {
        Self { task }
    }
}

impl SimpleFileOps for SetgroupsFile {
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        let task = upgrade_task(&self.task)?;
        let ns = task.as_thread().real_cred().user_ns_arc().clone();
        let text = if ns.setgroups_allowed() {
            "allow\n"
        } else {
            "deny\n"
        };
        Ok(Cow::Owned(text.as_bytes().to_vec()))
    }

    fn write(&self, file: &VfsFile, data: &[u8], offset: u64) -> VfsResult<usize> {
        const MAX_SETGROUPS_WRITE: usize = 8;

        // The pid file resolves its task before the write callback in Linux; a
        // task that vanished after open therefore reports ESRCH even for
        // malformed input.
        let task = upgrade_task(&self.task)?;
        let target_cred = task.as_thread().real_cred();
        let ns = target_cred.user_ns_arc().clone();
        let opener = file.cred();
        if !opener.has_cap_in(&ns, CAP_SETGID) {
            return Err(VfsError::OperationNotPermitted);
        }
        if offset != 0 || data.len() >= MAX_SETGROUPS_WRITE {
            return Err(VfsError::InvalidInput);
        }
        let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
        let (allowed, rest) = if let Some(rest) = text.strip_prefix("allow") {
            (true, rest)
        } else if let Some(rest) = text.strip_prefix("deny") {
            (false, rest)
        } else {
            return Err(VfsError::InvalidInput);
        };
        if !rest.trim_ascii().is_empty() {
            return Err(VfsError::InvalidInput);
        }

        ns.set_setgroups_allowed(allowed)?;
        Ok(data.len())
    }
}

fn parse_id_map(data: &[u8]) -> VfsResult<Vec<IdMapExtent>> {
    let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
    let mut extents = Vec::new();
    for line in text.split_terminator('\n') {
        let mut fields = line.split_ascii_whitespace();
        let inside = parse_id(fields.next())?;
        let outside = parse_id(fields.next())?;
        let count = parse_id(fields.next())?;
        if fields.next().is_some() {
            return Err(VfsError::InvalidInput);
        }
        // Linux rejects `(u32)-1` as a starting ID; it is the invalid-ID
        // sentinel rather than a mappable value. The count may still be
        // `u32::MAX`, which maps the whole 0..-2 range.
        if inside == u32::MAX || outside == u32::MAX {
            return Err(VfsError::InvalidInput);
        }
        extents.push(IdMapExtent::new(inside, outside, count)?);
    }
    Ok(extents)
}

fn parse_id(field: Option<&str>) -> VfsResult<u32> {
    let field = field.ok_or(VfsError::InvalidInput)?;
    if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(VfsError::InvalidInput);
    }
    field.parse::<u32>().map_err(|_| VfsError::InvalidInput)
}
