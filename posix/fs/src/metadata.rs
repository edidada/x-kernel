// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem metadata update syscalls.

use core::{
    ffi::{c_char, c_long},
    time::Duration,
};

use kcred::AccessIdKind;
use kerrno::{KError, KResult};
use khal::time::wall_time;
use kthread::with_current_credentials;
use kvfs::{Metadata, MetadataUpdate, NodePermission, Permission, check_permission};
use linux_raw_sys::general::*;
#[cfg(target_arch = "x86_64")]
use posix_types::utimbuf;
use posix_types::{TimeValueLike, UserConstPtr};

use crate::path::resolve_at;

/// `CAP_CHOWN` / `chgrp` policy before capabilities exist: `euid == 0` may
/// change any owner. Otherwise the caller must own the file (`fsuid`), may
/// not give the file away, and may only set a group they belong to.
fn require_chown_permission(
    meta: &Metadata,
    new_uid: Option<u32>,
    new_gid: Option<u32>,
) -> KResult<()> {
    with_current_credentials(|credentials| {
        if credentials.is_privileged() {
            return Ok(());
        }
        let fsuid = credentials.fsuid();
        let access = credentials.access_snapshot(AccessIdKind::Filesystem);
        if let Some(uid) = new_uid
            && (fsuid != meta.uid || uid != fsuid)
        {
            return Err(KError::OperationNotPermitted);
        }
        if let Some(gid) = new_gid
            && (fsuid != meta.uid || (gid != meta.gid && !access.has_group(gid)))
        {
            return Err(KError::OperationNotPermitted);
        }
        Ok(())
    })
}

/// `CAP_FOWNER` stand-in: only the file owner or a privileged caller may chmod.
fn require_chmod_permission(meta: &Metadata) -> KResult<()> {
    with_current_credentials(|credentials| {
        if credentials.is_privileged() || credentials.fsuid() == meta.uid {
            Ok(())
        } else {
            Err(KError::OperationNotPermitted)
        }
    })
}

/// Timestamp updates: owner or privileged caller always; setting both stamps
/// to "now" also allows write permission (`EACCES` if that DAC check fails).
fn require_utime_permission(meta: &Metadata, now_only: bool) -> KResult<()> {
    with_current_credentials(|credentials| {
        if credentials.is_privileged() || credentials.fsuid() == meta.uid {
            return Ok(());
        }
        if now_only {
            let access = credentials.access_snapshot(AccessIdKind::Filesystem);
            return check_permission(meta, Permission::MAY_WRITE, &access);
        }
        Err(KError::OperationNotPermitted)
    })
}

#[cfg(target_arch = "x86_64")]
pub fn sys_chown(path: UserConstPtr<c_char>, uid: i32, gid: i32) -> KResult<isize> {
    sys_fchownat(AT_FDCWD, path, uid, gid, 0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_lchown(path: UserConstPtr<c_char>, uid: i32, gid: i32) -> KResult<isize> {
    sys_fchownat(AT_FDCWD, path, uid, gid, AT_SYMLINK_NOFOLLOW)
}

pub fn sys_fchown(fd: i32, uid: i32, gid: i32) -> KResult<isize> {
    sys_fchownat(fd, UserConstPtr::default(), uid, gid, AT_EMPTY_PATH)
}

/// Changes file ownership relative to a directory file descriptor.
pub fn sys_fchownat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    uid: i32,
    gid: i32,
    flags: u32,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    let loc = resolve_at(dirfd, path.as_deref(), flags)?.into_location()?;
    let meta = loc.metadata()?;

    let new_uid = (uid != -1).then_some(uid as u32);
    let new_gid = (gid != -1).then_some(gid as u32);
    require_chown_permission(&meta, new_uid, new_gid)?;

    let mut mode = meta.mode;
    mode.remove(NodePermission::SET_UID);
    if mode.contains(NodePermission::GROUP_EXEC) {
        mode.remove(NodePermission::SET_GID);
    }

    let uid = new_uid.unwrap_or(meta.uid);
    let gid = new_gid.unwrap_or(meta.gid);
    loc.update_metadata(MetadataUpdate {
        owner: Some((uid, gid)),
        mode: Some(mode),
        ..Default::default()
    })?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_chmod(path: UserConstPtr<c_char>, mode: u32) -> KResult<isize> {
    sys_fchmodat(AT_FDCWD, path, mode, 0)
}

/// Changes file permissions by file descriptor.
pub fn sys_fchmod(fd: i32, mode: u32) -> KResult<isize> {
    sys_fchmodat(fd, UserConstPtr::default(), mode, AT_EMPTY_PATH)
}

/// Changes file permissions relative to a directory file descriptor.
pub fn sys_fchmodat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    mode: u32,
    flags: u32,
) -> KResult<isize> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    let loc = resolve_at(dirfd, path.as_deref(), flags)?.into_location()?;
    require_chmod_permission(&loc.metadata()?)?;
    loc.update_metadata(MetadataUpdate {
        mode: Some(NodePermission::from_bits_truncate(mode as u16)),
        ..Default::default()
    })?;
    Ok(0)
}

fn update_times(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    atime: Option<Duration>,
    mtime: Option<Duration>,
    flags: u32,
    now_only: bool,
) -> KResult<()> {
    let path = path
        .check_non_null()
        .map(UserConstPtr::load_string)
        .transpose()?;
    let loc = resolve_at(dirfd, path.as_deref(), flags)?.into_location()?;
    require_utime_permission(&loc.metadata()?, now_only)?;
    loc.update_metadata(MetadataUpdate {
        atime,
        mtime,
        ..Default::default()
    })?;
    Ok(())
}

#[cfg(target_arch = "x86_64")]
pub fn sys_utime(path: UserConstPtr<c_char>, times: UserConstPtr<utimbuf>) -> KResult<isize> {
    let (atime, mtime, now_only) = if let Some(times) = times.check_non_null() {
        let times = times.read_vm()?;
        (
            Duration::from_secs(times.actime as _),
            Duration::from_secs(times.modtime as _),
            false,
        )
    } else {
        let time = wall_time();
        (time, time, true)
    };
    update_times(AT_FDCWD, path, Some(atime), Some(mtime), 0, now_only)?;
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
pub fn sys_utimes(path: UserConstPtr<c_char>, times: UserConstPtr<[timeval; 2]>) -> KResult<isize> {
    let (atime, mtime, now_only) = if let Some(times) = times.check_non_null() {
        let [atime, mtime] = times.read_vm()?;
        (
            atime.try_into_time_value()?,
            mtime.try_into_time_value()?,
            false,
        )
    } else {
        let time = wall_time();
        (time, time, true)
    };
    update_times(AT_FDCWD, path, Some(atime), Some(mtime), 0, now_only)?;
    Ok(0)
}

pub fn sys_utimensat(
    dirfd: i32,
    path: UserConstPtr<c_char>,
    times: UserConstPtr<[timespec; 2]>,
    mut flags: u32,
) -> KResult<isize> {
    if path.is_null() {
        flags |= AT_EMPTY_PATH;
    }

    enum UtimeSpec {
        Omit,
        Now,
        Set(Duration),
    }

    fn utime_to_spec(time: &timespec) -> KResult<UtimeSpec> {
        match time.tv_nsec {
            val if val == UTIME_OMIT as c_long => Ok(UtimeSpec::Omit),
            val if val == UTIME_NOW as c_long => Ok(UtimeSpec::Now),
            _ => Ok(UtimeSpec::Set(time.try_into_time_value()?)),
        }
    }

    fn spec_to_duration(spec: UtimeSpec, now: Duration) -> Option<Duration> {
        match spec {
            UtimeSpec::Omit => None,
            UtimeSpec::Now => Some(now),
            UtimeSpec::Set(duration) => Some(duration),
        }
    }

    let (atime, mtime, now_only) = if let Some(times) = times.check_non_null() {
        let [atime, mtime] = times.read_vm()?;
        let atime = utime_to_spec(&atime)?;
        let mtime = utime_to_spec(&mtime)?;
        if matches!((&atime, &mtime), (UtimeSpec::Omit, UtimeSpec::Omit)) {
            return Ok(0);
        }
        let now_only = matches!((&atime, &mtime), (UtimeSpec::Now, UtimeSpec::Now));
        let now = wall_time();
        (
            spec_to_duration(atime, now),
            spec_to_duration(mtime, now),
            now_only,
        )
    } else {
        let time = wall_time();
        (Some(time), Some(time), true)
    };
    update_times(dirfd, path, atime, mtime, flags, now_only)?;
    Ok(0)
}
