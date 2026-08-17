// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory file descriptor syscalls.

use alloc::{format, sync::Arc};
use core::ffi::c_char;

use kerrno::{KError, KResult};
use kfs::OpenOptions;
use kthread::{current_process_fs_context, current_process_state};
use kvfs::VfsError;
use linux_raw_sys::general::{MFD_CLOEXEC, O_RDWR};
use posix_types::UserConstPtr;

// TODO: correct memfd implementation

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    struct MemfdFlags: u32 {
        const CLOEXEC = MFD_CLOEXEC;
    }
}

impl MemfdFlags {
    fn from_raw(bits: u32) -> KResult<Self> {
        Self::from_bits(bits).ok_or(KError::InvalidInput)
    }

    fn is_cloexec(self) -> bool {
        self.contains(Self::CLOEXEC)
    }
}

/// Creates an anonymous in-memory file descriptor.
///
/// Candidate backing names are claimed with create-exclusive semantics, so
/// concurrent callers cannot open or reuse a file selected by another caller.
///
/// # Errors
///
/// Returns [`KError::InvalidInput`] for unsupported flags, propagates backing
/// filesystem and descriptor-table errors, and returns
/// [`KError::TooManyOpenFiles`] when all candidate names are occupied.
pub fn sys_memfd_create(_name: UserConstPtr<c_char>, flags: u32) -> KResult<isize> {
    let flags = MemfdFlags::from_raw(flags)?;
    let fs = current_process_fs_context().lock().clone();

    for id in 0..0xffff {
        let name = format!("/tmp/memfd-{id:04x}");
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .create_new(true)
            .open_flags(O_RDWR)
            .open(&fs, &name)
        {
            Ok(file) => file,
            Err(VfsError::AlreadyExists) => continue,
            Err(err) => return Err(err),
        };
        let proc_state = current_process_state();
        return proc_state
            .resources
            .add_file_like(Arc::new(file), flags.is_cloexec())
            .map(|fd| fd as _);
    }
    Err(KError::TooManyOpenFiles)
}
