// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inotify syscall handlers.

use core::ffi::c_char;

use kerrno::{KError, KResult};
use kfd_objects::inotify::Inotify;
use linux_raw_sys::general::{
    IN_CLOEXEC, IN_DONT_FOLLOW, IN_EXCL_UNLINK, IN_MASK_ADD, IN_MASK_CREATE, IN_NONBLOCK,
    IN_ONESHOT, IN_ONLYDIR,
};
use posix_types::UserConstPtr;

/// Bits the kernel accepts in a watch mask: event bits plus control bits that
/// are legal to request. Output-only bits such as `IN_ISDIR` or `IN_UNMOUNT`
/// are not valid input and are rejected.
const IN_VALID_MASK_BITS: u32 = IN_ALL_EVENTS
    | IN_ONLYDIR
    | IN_DONT_FOLLOW
    | IN_EXCL_UNLINK
    | IN_MASK_CREATE
    | IN_MASK_ADD
    | IN_ONESHOT;

/// Creates an inotify instance.
pub fn sys_inotify_init1(flags: u32) -> KResult<isize> {
    debug!("inotify_init1 <= flags: {flags:#x}");

    let unknown = flags & !(IN_CLOEXEC | IN_NONBLOCK);
    if unknown != 0 {
        return Err(KError::InvalidInput);
    }

    let file = Inotify::new_file(IN_NONBLOCK & flags, kprocess::current_cred())?;
    kprocess::current_resources()
        .add_file(file, flags & IN_CLOEXEC != 0)
        .map(|fd| fd as _)
}

/// Adds a watch on `pathname` to the inotify instance referred to by `fd`.
pub fn sys_inotify_add_watch(fd: i32, pathname: UserConstPtr<c_char>, mask: u32) -> KResult<isize> {
    let pathname = pathname.load_string()?;
    debug!("inotify_add_watch <= fd: {fd}, pathname: {pathname}, mask: {mask:#x}");

    let file = kprocess::current_resources().get_file(fd)?;
    let inotify = Inotify::from_file(&file)?;

    if mask & !IN_VALID_MASK_BITS != 0 {
        return Err(KError::InvalidInput);
    }

    inotify.add_watch(pathname.as_str(), mask).map(|wd| wd as _)
}

/// Removes the watch `wd` from the inotify instance referred to by `fd`.
pub fn sys_inotify_rm_watch(fd: i32, wd: i32) -> KResult<isize> {
    debug!("inotify_rm_watch <= fd: {fd}, wd: {wd}");

    let file = kprocess::current_resources().get_file(fd)?;
    let inotify = Inotify::from_file(&file)?;

    inotify.rm_watch(wd).map(|_| 0)
}

use linux_raw_sys::general::IN_ALL_EVENTS;
