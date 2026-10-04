// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-visible ABI of the async submission/completion rings.
//!
//! All layouts are kernel-defined (io_uring *semantics*, not ABI) and must be
//! padding-free because they cross the user boundary through
//! [`posix_types::UserRead`]/[`posix_types::UserWrite`]. The static asserts at
//! the bottom of this file pin every size and stride.
//!
//! The ABI ships complete rather than incrementally, so items that the not
//! yet landed ring/syscall steps will consume are staged here. Allow the
//! resulting dead-code findings only in unittest builds, where the syscall
//! consumers are absent; in plain builds the `expect(dead_code)` on the
//! module declaration accounts for them instead.
#![cfg_attr(unittest, allow(dead_code))]

use core::mem::{offset_of, size_of};

use posix_types::{UserConstPtr, UserPtr, UserRead, UserWrite};

/// Async read from `fd` into the buffer at `addr`.
pub const ASYNCIO_OP_READ: u8 = 1;
/// Async write from the buffer at `addr` to `fd`.
pub const ASYNCIO_OP_WRITE: u8 = 2;
/// Async `fsync` of `fd` (full sync, `data_only = false`).
pub const ASYNCIO_OP_FSYNC: u8 = 3;
/// Async `splice` between `fd` (source) and `fd2` (destination).
pub const ASYNCIO_OP_SPLICE: u8 = 4;
/// Async `sendfile` from `fd` (source) to `fd2` (destination).
pub const ASYNCIO_OP_SENDFILE: u8 = 5;
/// Async `copy_file_range` between `fd` (source) and `fd2` (destination).
pub const ASYNCIO_OP_COPY_FILE_RANGE: u8 = 6;

/// `AsyncSqe.flags`: `off` is a valid absolute offset for READ/WRITE.
///
/// Without this bit, READ/WRITE use (and advance) the file's current
/// position. Splice-family operations instead carry user-space offset
/// *pointers* in `off`/`off2`, where a null pointer means "current position"
/// — matching the Linux syscall signatures — so this flag does not apply.
pub const ASYNCIO_SQE_OFF_VALID: u8 = 1 << 0;

/// `AsyncSqe.flags`: `fd2` is a valid destination fd (splice family).
pub const ASYNCIO_SQE_FD2_VALID: u8 = 1 << 1;

/// All SQE flag bits defined so far; anything else is rejected.
pub const ASYNCIO_SQE_FLAGS_KNOWN: u8 = ASYNCIO_SQE_OFF_VALID | ASYNCIO_SQE_FD2_VALID;

/// `AsyncioParams.flags` accepted by the setup entry point (landed with the
/// `asyncio_setup` syscall step).
pub const ASYNCIO_PARAMS_VERSION: u32 = 0;

/// `enter` flag: block until `min_complete` completions are published.
pub const ASYNCIO_ENTER_WAIT: u32 = 1 << 0;
/// `enter` flag: bound the WAIT by the timeout at `argp`
/// (`*const __kernel_timespec`). Requires [`ASYNCIO_ENTER_WAIT`].
pub const ASYNCIO_ENTER_TIMEOUT: u32 = 1 << 1;

/// All `enter` flag bits defined so far; anything else is `EINVAL`.
pub const ASYNCIO_ENTER_FLAGS_KNOWN: u32 = ASYNCIO_ENTER_WAIT | ASYNCIO_ENTER_TIMEOUT;

/// Ring header field byte offsets inside the shared ring memory.
///
/// Every field is a `u32`; the header is 4K-aligned and far smaller than a
/// page, so no atomic counter ever straddles a page boundary.
pub mod header {
    /// Submission ring: kernel-owned consumed index (free-running).
    pub const SQ_HEAD: usize = 0x00;
    /// Submission ring: user-owned published index (free-running).
    pub const SQ_TAIL: usize = 0x04;
    /// Submission ring: entry index mask (`sq_entries - 1`).
    pub const SQ_MASK: usize = 0x08;
    /// Submission ring: entry count.
    pub const SQ_ENTRIES: usize = 0x0C;
    /// Completion ring: user-owned consumed index (free-running).
    pub const CQ_HEAD: usize = 0x10;
    /// Completion ring: kernel-owned published index (free-running).
    pub const CQ_TAIL: usize = 0x14;
    /// Completion ring: entry index mask (`cq_entries - 1`).
    pub const CQ_MASK: usize = 0x18;
    /// Completion ring: entry count.
    pub const CQ_ENTRIES: usize = 0x1C;
    /// Context status flags (see [`CTX_FLAG_SHUTDOWN`]).
    pub const CTX_FLAGS: usize = 0x20;
    /// Size of the ring header (SQE array starts here).
    pub const LEN: usize = 0x40;

    /// Context status: the context is shutting down (fd released).
    pub const CTX_FLAG_SHUTDOWN: u32 = 1 << 0;
}

/// Byte size of one [`AsyncSqe`].
pub const ASYNCIO_SQE_SIZE: usize = 64;
/// Byte size of one [`AsyncCqe`].
pub const ASYNCIO_CQE_SIZE: usize = 32;
/// Maximum supported submission-ring depth.
pub const ASYNCIO_MAX_SQ_ENTRIES: u32 = 4096;

/// CQE `res` for an operation discarded before it started: negated Linux
/// `ECANCELED` (125 on the generic UAPI errno table; `kerrno` carries no
/// matching variant yet).
pub const ASYNCIO_RES_CANCELED: i64 = -(linux_raw_sys::errno::ECANCELED as i64);

/// One submission-queue entry (64 bytes).
///
/// Field roles depend on `opcode`:
///
/// | opcode | fd | fd2 | addr | len | off | off2 |
/// |---|---|---|---|---|---|---|
/// | READ/WRITE | target | — | user buffer | byte count | absolute offset if [`ASYNCIO_SQE_OFF_VALID`] | — |
/// | FSYNC | target | — | — | — | — | — |
/// | SPLICE | source fd | dest fd (needs [`ASYNCIO_SQE_FD2_VALID`]) | — | byte count | `*i64` source offset ptr | `*i64` dest offset ptr |
/// | SENDFILE | source fd | dest fd | — | byte count | `*u64` source offset ptr | — |
/// | COPY_FILE_RANGE | source fd | dest fd | — | byte count | `*u64` source offset ptr | `*u64` dest offset ptr |
///
/// Null offset pointers mean "use the file's current position", matching the
/// Linux syscall signatures.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct AsyncSqe {
    /// One of the `ASYNCIO_OP_*` opcodes.
    pub opcode: u8,
    /// [`ASYNCIO_SQE_OFF_VALID`] / [`ASYNCIO_SQE_FD2_VALID`].
    pub flags: u8,
    /// Must be zero.
    pub reserved0: u16,
    /// Source/target file descriptor.
    pub fd: i32,
    /// User cookie echoed verbatim in the completion entry.
    pub cookie: u64,
    /// Opcode-dependent user address (see the table above).
    pub addr: u64,
    /// Byte count for data-movement opcodes.
    pub len: u64,
    /// Destination file descriptor when [`ASYNCIO_SQE_FD2_VALID`] is set.
    pub fd2: i32,
    /// Must be zero.
    pub reserved1: u32,
    /// Opcode-dependent offset (see the table above).
    pub off: u64,
    /// Second offset pointer for two-ended transfers.
    pub off2: u64,
    /// Must be zero.
    pub reserved2: u64,
}

/// One completion-queue entry (32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct AsyncCqe {
    /// Cookie echoed from the originating SQE.
    pub cookie: u64,
    /// Operation result: non-negative byte/count value, or negated errno.
    pub res: i64,
    /// Reserved; always written as zero by the kernel. Keeps the entry at
    /// 32 bytes so no CQE ever straddles a page in the shared ring.
    pub reserved0: u64,
    /// Reserved; always written as zero by the kernel.
    pub flags: u32,
    /// Explicit tail padding, always zero (the `k_epoll_event` rule).
    pub abi_pad: u32,
}

/// Setup parameter block for `asyncio_setup` (40 bytes, in/out).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct AsyncioParams {
    /// In: must be [`ASYNCIO_PARAMS_VERSION`].
    pub version: u32,
    /// In: reserved, must be zero.
    pub flags: u32,
    /// Out: submission-ring entry count.
    pub sq_entries: u32,
    /// Out: completion-ring entry count.
    pub cq_entries: u32,
    /// Out: user VA of the ring (header + SQE array + CQE array).
    pub ring_user_va: usize,
    /// Out: mapped byte length of the ring.
    pub ring_user_len: usize,
    /// Out: [`ASYNCIO_SQE_SIZE`].
    pub sqe_size: u32,
    /// Out: [`ASYNCIO_CQE_SIZE`].
    pub cqe_size: u32,
}

impl AsyncSqe {
    /// Returns `true` when the SQE carries no unknown flags or nonzero
    /// reserved fields.
    pub fn is_abi_clean(&self) -> bool {
        self.flags & !ASYNCIO_SQE_FLAGS_KNOWN == 0
            && self.reserved0 == 0
            && self.reserved1 == 0
            && self.reserved2 == 0
    }

    /// Returns `true` when READ/WRITE must use `off` instead of the file
    /// position.
    pub fn has_explicit_offset(&self) -> bool {
        self.flags & ASYNCIO_SQE_OFF_VALID != 0
    }

    /// Returns the destination fd when [`ASYNCIO_SQE_FD2_VALID`] is set.
    pub fn dest_fd(&self) -> Option<i32> {
        (self.flags & ASYNCIO_SQE_FD2_VALID != 0).then_some(self.fd2)
    }

    /// Reinterprets `off` as the source offset pointer of a transfer.
    pub fn off_source_ptr(&self) -> UserConstPtr<u64> {
        UserConstPtr::from(self.off as usize)
    }

    /// Reinterprets `off` as the source `i64` offset pointer of a splice.
    pub fn off_source_i64_ptr(&self) -> UserConstPtr<i64> {
        UserConstPtr::from(self.off as usize)
    }

    /// Reinterprets `off2` as the destination offset pointer of a transfer.
    pub fn off_dest_ptr(&self) -> UserConstPtr<u64> {
        UserConstPtr::from(self.off2 as usize)
    }

    /// Reinterprets `off2` as the destination `i64` offset pointer of a splice.
    pub fn off_dest_i64_ptr(&self) -> UserConstPtr<i64> {
        UserConstPtr::from(self.off2 as usize)
    }

    /// Reinterprets `addr` as the mutable user buffer of READ.
    pub fn read_buffer(&self) -> UserPtr<u8> {
        UserPtr::from(self.addr as usize)
    }

    /// Reinterprets `addr` as the immutable user buffer of WRITE.
    pub fn write_buffer(&self) -> UserConstPtr<u8> {
        UserConstPtr::from(self.addr as usize)
    }
}

const _: () = {
    assert!(size_of::<AsyncSqe>() == ASYNCIO_SQE_SIZE);
    assert!(offset_of!(AsyncSqe, reserved1) == 36);
    assert!(offset_of!(AsyncSqe, off) == 40);
    assert!(offset_of!(AsyncSqe, off2) == 48);
    assert!(offset_of!(AsyncSqe, reserved2) == 56);
    assert!(size_of::<AsyncCqe>() == ASYNCIO_CQE_SIZE);
    assert!(offset_of!(AsyncCqe, res) == 8);
    assert!(offset_of!(AsyncCqe, reserved0) == 16);
    assert!(offset_of!(AsyncCqe, flags) == 24);
    assert!(offset_of!(AsyncCqe, abi_pad) == 28);
    assert!(size_of::<AsyncioParams>() == 40);
    assert!(offset_of!(AsyncioParams, ring_user_va) == 16);
    assert!(size_of::<u64>() == 8);
};
