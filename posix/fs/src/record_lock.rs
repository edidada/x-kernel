// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux process-associated advisory record-lock syscall boundary.

use alloc::sync::Arc;
use core::ffi::c_int;

use kerrno::{KError, KResult, LinuxError};
use kvfs::{
    FMode, VfsFile,
    record_lock::{RecordLockKind, RecordLockRange},
};
use linux_raw_sys::general::*;
use posix_types::{UserPtr, fs::k_flock64};

fn normalize_range(base: u64, start: i64, len: i64) -> KResult<RecordLockRange> {
    let start = i128::from(base) + i128::from(start);
    if start < 0 {
        return Err(KError::InvalidInput);
    }
    if start > i128::from(i64::MAX) {
        return Err(LinuxError::EOVERFLOW.into());
    }
    let (start, end) = match len.cmp(&0) {
        core::cmp::Ordering::Greater => (start, start + i128::from(len) - 1),
        core::cmp::Ordering::Less => (start + i128::from(len), start - 1),
        core::cmp::Ordering::Equal => (start, i128::from(i64::MAX)),
    };
    if start < 0 || end < 0 {
        return Err(KError::InvalidInput);
    }
    if end > i128::from(i64::MAX) {
        return Err(LinuxError::EOVERFLOW.into());
    }
    RecordLockRange::new(start as u64, end as u64)
}

fn request(file: &VfsFile, lock: &k_flock64) -> KResult<(RecordLockRange, Option<RecordLockKind>)> {
    let base = match u32::try_from(lock.l_whence) {
        Ok(SEEK_SET) => 0,
        Ok(SEEK_CUR) => file.position(),
        Ok(SEEK_END) => file.size(),
        _ => return Err(KError::InvalidInput),
    };
    let range = normalize_range(base, lock.l_start, lock.l_len)?;
    let kind = match u32::try_from(lock.l_type) {
        Ok(F_RDLCK) => Some(RecordLockKind::Read),
        Ok(F_WRLCK) => Some(RecordLockKind::Write),
        Ok(F_UNLCK) => None,
        _ => return Err(KError::InvalidInput),
    };
    Ok((range, kind))
}

pub(super) fn fcntl_record_lock(fd: c_int, cmd: u32, arg: usize) -> KResult<isize> {
    let resources = kprocess::current_resources();
    let table = resources.fd_table()?;
    let file = table.read().get_file(fd)?;
    if file.mode().contains(FMode::PATH) {
        return Err(KError::BadFileDescriptor);
    }
    let ptr = UserPtr::<k_flock64>::from(arg);
    let mut lock = ptr.read_vm()?;
    let (range, kind) = request(&file, &lock)?;
    if cmd == F_GETLK {
        let kind = kind.ok_or(KError::InvalidInput)?;
        let conflict = table
            .read()
            .record_lock_owner()
            .conflict(file.inode(), range, kind);
        if let Some(conflict) = conflict {
            lock.l_type = match conflict.kind {
                RecordLockKind::Read => F_RDLCK,
                RecordLockKind::Write => F_WRLCK,
            } as _;
            lock.l_whence = SEEK_SET as _;
            lock.l_start = conflict.range.start() as _;
            lock.l_len = if conflict.range.end() == i64::MAX as u64 {
                0
            } else {
                (conflict.range.end() - conflict.range.start() + 1) as _
            };
            lock.l_pid = conflict.pid;
        } else {
            lock.l_type = F_UNLCK as _;
        }
        ptr.write_vm(lock)?;
        return Ok(0);
    }
    match kind {
        Some(RecordLockKind::Read) if !file.mode().contains(FMode::READ) => {
            return Err(KError::BadFileDescriptor);
        }
        Some(RecordLockKind::Write) if !file.mode().contains(FMode::WRITE) => {
            return Err(KError::BadFileDescriptor);
        }
        _ => {}
    }
    let pid = kprocess::current_user_process().pid() as i32;
    loop {
        let wait = {
            // Close and dup replacement use the write side of this lock. Recheck
            // the descriptor after every wait before publishing an acquired lock.
            let descriptors = table.read();
            let current_file = descriptors.get_file(fd)?;
            if !Arc::ptr_eq(&current_file, &file) {
                return Err(KError::BadFileDescriptor);
            }
            descriptors
                .record_lock_owner()
                .set(file.inode(), range, kind, pid, cmd == F_SETLKW)?
        };
        let Some(wait) = wait else {
            return Ok(0);
        };
        match wait.wait() {
            Err(KError::Interrupted) => return Err(LinuxError::ERESTARTSYS.into()),
            result => result?,
        }
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn record_lock_normalizes_relative_and_reverse_ranges() {
        let range = normalize_range(100, -20, 10).unwrap();
        assert_eq!((range.start(), range.end()), (80, 89));
        let range = normalize_range(100, 20, -30).unwrap();
        assert_eq!((range.start(), range.end()), (90, 119));
        let range = normalize_range(100, 20, 0).unwrap();
        assert_eq!((range.start(), range.end()), (120, i64::MAX as u64));
    }

    #[def_test]
    fn record_lock_rejects_negative_offsets_and_overflow() {
        assert_eq!(normalize_range(0, -1, 1), Err(KError::InvalidInput));
        assert_eq!(normalize_range(0, 0, -1), Err(KError::InvalidInput));
        assert_eq!(normalize_range(0, 2, -3), Err(KError::InvalidInput));
        assert_eq!(
            normalize_range(i64::MAX as u64, 1, 1),
            Err(LinuxError::EOVERFLOW.into())
        );
        assert_eq!(
            normalize_range(0, i64::MAX, 2),
            Err(LinuxError::EOVERFLOW.into())
        );
        assert_eq!(
            normalize_range(0, i64::MAX, 1).unwrap().end(),
            i64::MAX as u64
        );
    }
}
