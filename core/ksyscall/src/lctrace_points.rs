// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LCTrace probes on the syscall dispatch path.
//!
//! Each probe has a real body when the `lctrace` feature is on and an empty
//! one otherwise, so the dispatcher stays free of `cfg` attributes.
//!
//! `SyscallEntry` carries the raw syscall number in argument 0 so a snapshot
//! can decode it back to a name, which is why the number is passed as `usize`
//! rather than as a `Sysno`.

/// Records a syscall about to be dispatched, with its first three arguments.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn syscall_entry(sysno: usize, arg0: usize, arg1: usize, arg2: usize) {
    lctrace::event!(SyscallEntry, sysno, arg0, arg1, arg2);
}

/// Records the value a syscall is returning to user space.
#[cfg(feature = "lctrace")]
#[inline]
pub(crate) fn syscall_exit(sysno: usize, retval: isize) {
    lctrace::event!(SyscallExit, sysno, retval);
}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn syscall_entry(_sysno: usize, _arg0: usize, _arg1: usize, _arg2: usize) {}

#[cfg(not(feature = "lctrace"))]
#[inline]
pub(crate) fn syscall_exit(_sysno: usize, _retval: isize) {}
