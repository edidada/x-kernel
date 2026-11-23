// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The dma-buf userspace ABI.
//!
//! The command numbers and structure layouts are Linux's
//! (`include/uapi/linux/dma-buf.h`). `_IOC` folds the argument size into the
//! request code, so a drifted field is a *different command* rather than a
//! misaligned argument: the descriptor then answers "not supported" instead of
//! reading the wrong bytes. The `size_of` and request-number assertions below
//! keep that property at compile time.

use bytemuck::AnyBitPattern;
use posix_types::{UserRead, UserWrite};

use crate::object::CpuAccess;

// ---------------------------------------------------------------------------
// Request encoding
// ---------------------------------------------------------------------------

const IOC_READ: u32 = 2;
const IOC_WRITE: u32 = 1;

/// One ioctl number built from its direction, type, sequence number and
/// argument size, as `_IOC` computes it in `asm-generic/ioctl.h`.
const fn ioc(dir: u32, ty: u8, nr: u8, size: u16) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | (nr as u32)
}

/// `_IOW` over an argument type.
const fn iow<T>(ty: u8, nr: u8) -> u32 {
    ioc(IOC_WRITE, ty, nr, core::mem::size_of::<T>() as u16)
}

/// `_IOWR` over an argument type.
const fn iowr<T>(ty: u8, nr: u8) -> u32 {
    ioc(
        IOC_READ | IOC_WRITE,
        ty,
        nr,
        core::mem::size_of::<T>() as u16,
    )
}

/// The dma-buf command type (`DMA_BUF_BASE`, `'b'`).
const DMA_BUF_BASE: u8 = b'b';

// ---------------------------------------------------------------------------
// Structures
// ---------------------------------------------------------------------------

/// `struct dma_buf_sync` — the argument of [`DMA_BUF_IOCTL_SYNC`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DmaBufSync {
    /// A [`DMA_BUF_SYNC_*`](DMA_BUF_SYNC_READ) combination.
    pub flags: u64,
}

/// `struct dma_buf_export_sync_file` — the argument of
/// [`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DmaBufExportSyncFile {
    /// The waiting usage the exported fence orders.
    pub flags: u32,
    /// [out] The sync_file descriptor.
    pub fd: i32,
}

/// `struct dma_buf_import_sync_file` — the argument of
/// [`DMA_BUF_IOCTL_IMPORT_SYNC_FILE`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, AnyBitPattern, UserRead, UserWrite)]
pub struct DmaBufImportSyncFile {
    /// The usage the imported fence orders.
    pub flags: u32,
    /// [in] The sync_file descriptor to import.
    pub fd: i32,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// `DMA_BUF_IOCTL_SYNC`: begin or end CPU access.
pub const DMA_BUF_IOCTL_SYNC: u32 = iow::<DmaBufSync>(DMA_BUF_BASE, 0x0);

/// `DMA_BUF_IOCTL_SET_NAME`: rename the buffer for diagnostics.
///
/// Linux spells this command once per user-pointer width, so both sizes are
/// accepted; the kernel reads a string from the pointer either way.
pub const DMA_BUF_IOCTL_SET_NAME: u32 = iow::<u64>(DMA_BUF_BASE, 0x1);
/// The 32-bit-pointer spelling of [`DMA_BUF_IOCTL_SET_NAME`].
pub const DMA_BUF_IOCTL_SET_NAME_U32: u32 = iow::<u32>(DMA_BUF_BASE, 0x1);

/// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`: export the buffer's fence.
///
/// This is the one `_IOWR` command of the set: the kernel writes the new
/// descriptor number back into the argument.
pub const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: u32 = iowr::<DmaBufExportSyncFile>(DMA_BUF_BASE, 0x2);

/// `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`: record a sync_file's fence.
pub const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: u32 = iow::<DmaBufImportSyncFile>(DMA_BUF_BASE, 0x3);

// The numbers are the ABI. Pinning them here makes a layout or encoding
// mistake a compile error instead of a silently different command.
const _: () = assert!(core::mem::size_of::<DmaBufSync>() == 8);
const _: () = assert!(core::mem::size_of::<DmaBufExportSyncFile>() == 8);
const _: () = assert!(core::mem::size_of::<DmaBufImportSyncFile>() == 8);
const _: () = assert!(DMA_BUF_IOCTL_SYNC == 0x4008_6200);
const _: () = assert!(DMA_BUF_IOCTL_SET_NAME == 0x4008_6201);
const _: () = assert!(DMA_BUF_IOCTL_SET_NAME_U32 == 0x4004_6201);
const _: () = assert!(DMA_BUF_IOCTL_EXPORT_SYNC_FILE == 0xc008_6202);
const _: () = assert!(DMA_BUF_IOCTL_IMPORT_SYNC_FILE == 0x4008_6203);

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

/// The CPU is about to read the buffer (`DMA_BUF_SYNC_READ`).
pub const DMA_BUF_SYNC_READ: u64 = 1 << 0;
/// The CPU is about to write the buffer (`DMA_BUF_SYNC_WRITE`).
pub const DMA_BUF_SYNC_WRITE: u64 = 1 << 1;
/// Both directions (`DMA_BUF_SYNC_RW`).
pub const DMA_BUF_SYNC_RW: u64 = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE;
/// CPU access begins (`DMA_BUF_SYNC_START`, which is zero).
pub const DMA_BUF_SYNC_START: u64 = 0 << 2;
/// CPU access ends (`DMA_BUF_SYNC_END`).
pub const DMA_BUF_SYNC_END: u64 = 1 << 2;

/// Every bit Linux accepts in `dma_buf_sync.flags`.
///
/// `DMA_BUF_SYNC_NOWAIT` (`1 << 4`) is an Android-era bit outside this mask;
/// Linux rejects it and so does [`classify_sync`].
pub const DMA_BUF_SYNC_VALID_FLAGS: u64 = DMA_BUF_SYNC_RW | DMA_BUF_SYNC_END;

/// The longest name `DMA_BUF_SET_NAME` accepts (`DMA_BUF_NAME_LEN`).
pub const DMA_BUF_NAME_LEN: usize = 32;

/// One validated `DMA_BUF_IOCTL_SYNC` request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SyncRequest {
    /// Direction of the CPU access.
    pub(crate) access: CpuAccess,
    /// Whether this request ends the access (`DMA_BUF_SYNC_END`).
    pub(crate) is_end: bool,
}

/// Validates `dma_buf_sync.flags` and classifies the request.
///
/// A bit outside [`DMA_BUF_SYNC_VALID_FLAGS`] is invalid, and so is a request
/// that names no direction at all: `dma_buf_ioctl` answers `EINVAL` for both
/// (there is no "sync everything" spelling). `DMA_BUF_SYNC_RW` is a valid
/// bidirectional access.
pub(crate) const fn classify_sync(flags: u64) -> Option<SyncRequest> {
    if flags & !DMA_BUF_SYNC_VALID_FLAGS != 0 {
        return None;
    }
    let access = match flags & DMA_BUF_SYNC_RW {
        DMA_BUF_SYNC_READ => CpuAccess::Read,
        DMA_BUF_SYNC_WRITE => CpuAccess::Write,
        DMA_BUF_SYNC_RW => CpuAccess::ReadWrite,
        _ => return None,
    };
    Some(SyncRequest {
        access,
        is_end: flags & DMA_BUF_SYNC_END != 0,
    })
}

unittest::path_mod_test!(tests, "../ut-tests/sync.rs");
