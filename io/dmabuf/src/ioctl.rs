// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Descriptor commands (`dma_buf_ioctl`).
//!
//! The descriptor owns the commands every export shares — CPU access
//! (`DMA_BUF_IOCTL_SYNC`) and the diagnostics-only rename
//! (`DMA_BUF_IOCTL_SET_NAME`) — and forwards the two reservation commands to
//! the export, because only an export with fences can answer them.
//!
//! CPU access is a pair of calls on the object, not cache maintenance here:
//! [`DmaBuf::begin_cpu_access`] and [`DmaBuf::end_cpu_access`] are where an
//! exporter decides what its buffer needs (a data-cache operation for a
//! cacheable heap buffer, a fence wait for a GPU buffer, nothing for a
//! coherent one).

use alloc::string::String;

use kvfs::{VfsError, VfsFile, VfsResult};
use posix_types::{UserConstPtr, UserPtr};

use crate::{
    file::object_of,
    object::DmaBuf,
    uapi::{
        DMA_BUF_IOCTL_EXPORT_SYNC_FILE, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, DMA_BUF_IOCTL_SET_NAME,
        DMA_BUF_IOCTL_SET_NAME_U32, DMA_BUF_IOCTL_SYNC, DMA_BUF_NAME_LEN, DmaBufSync,
        classify_sync,
    },
};

/// Handles one command issued on a dma-buf descriptor.
pub(crate) fn handle(file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
    let object = object_of(file)?;
    match cmd {
        DMA_BUF_IOCTL_SYNC => sync(&*object, arg),
        DMA_BUF_IOCTL_SET_NAME | DMA_BUF_IOCTL_SET_NAME_U32 => set_name(&*object, arg),
        // The reservation commands are the export's: a buffer without fences
        // answers `OperationNotSupported` through the trait's default.
        DMA_BUF_IOCTL_EXPORT_SYNC_FILE | DMA_BUF_IOCTL_IMPORT_SYNC_FILE => {
            object.sync_file_ioctl(cmd, arg)
        }
        // `dma_buf_ioctl` answers `ENOTTY` for a command it does not
        // implement, which is also how a client probes for newer commands.
        _ => Err(VfsError::NotATty),
    }
}

/// `DMA_BUF_IOCTL_SYNC`: acquire the buffer for CPU access, or publish what
/// the CPU wrote.
fn sync(object: &dyn DmaBuf, arg: usize) -> VfsResult<usize> {
    let request: DmaBufSync = UserPtr::<DmaBufSync>::from(arg)
        .read_vm()
        .map_err(|_| VfsError::BadAddress)?;
    let Some(sync) = classify_sync(request.flags) else {
        return Err(VfsError::InvalidInput);
    };
    if sync.is_end {
        object.end_cpu_access(sync.access)?;
    } else {
        object.begin_cpu_access(sync.access)?;
    }
    Ok(0)
}

/// `DMA_BUF_IOCTL_SET_NAME`: rename the buffer for diagnostics.
fn set_name(object: &dyn DmaBuf, arg: usize) -> VfsResult<usize> {
    let bytes = UserConstPtr::<u8>::from(arg)
        .cast::<core::ffi::c_char>()
        // `DMA_BUF_NAME_LEN` counts the terminator, as `strndup_user(buf, 32)`
        // does: at most 31 characters are accepted and byte 32 is never read.
        // The helper's bound is the payload length instead, so it gets one
        // byte less.
        .load_bytes_with_max_len(DMA_BUF_NAME_LEN - 1)
        // `strndup_user` reports an over-long name and a bad pointer
        // differently: `EINVAL` for the former, `EFAULT` for the latter.
        .map_err(|error| match error.canonicalize() {
            kerrno::KError::OutOfRange => VfsError::InvalidInput,
            _ => error,
        })?;
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    object.set_name(&String::from_utf8_lossy(&bytes[..end]));
    Ok(0)
}
