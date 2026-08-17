// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal inotify syscall compatibility.
//!
//! X-Kernel does not yet have VFS change-notification hooks.  These entry
//! points nevertheless provide a pollable, permanently idle inotify instance
//! so applications which use watches for optional live reload can run.  No
//! events are generated until the VFS notification backend is implemented.

use core::{
    ffi::c_char,
    sync::atomic::{AtomicI32, Ordering},
};

use kerrno::{KError, KResult};
use linux_raw_sys::general::{AT_FDCWD, EFD_CLOEXEC, EFD_NONBLOCK};
use posix_types::UserConstPtr;

use super::sys_eventfd2;

static NEXT_WATCH_DESCRIPTOR: AtomicI32 = AtomicI32::new(1);

/// Creates a pollable inotify-compatible descriptor.
pub fn sys_inotify_init1(flags: u32) -> KResult<isize> {
    const VALID_FLAGS: u32 = EFD_CLOEXEC | EFD_NONBLOCK;

    if flags & !VALID_FLAGS != 0 {
        return Err(KError::InvalidInput);
    }
    sys_eventfd2(0, flags)
}

/// Registers an idle compatibility watch after validating its inputs.
pub fn sys_inotify_add_watch(fd: i32, path: UserConstPtr<c_char>, _mask: u32) -> KResult<isize> {
    kprocess::current_resources().get_file(fd)?;
    posix_fs::sys_faccessat(AT_FDCWD, path, 0)?;

    let wd = NEXT_WATCH_DESCRIPTOR.fetch_add(1, Ordering::Relaxed);
    if wd <= 0 {
        NEXT_WATCH_DESCRIPTOR.store(2, Ordering::Relaxed);
        Ok(1)
    } else {
        Ok(wd as _)
    }
}

/// Removes a compatibility watch.
pub fn sys_inotify_rm_watch(fd: i32, wd: i32) -> KResult<isize> {
    kprocess::current_resources().get_file(fd)?;
    if wd <= 0 {
        return Err(KError::InvalidInput);
    }
    Ok(0)
}
