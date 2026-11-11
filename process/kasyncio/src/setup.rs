// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Async-context creation: allocates the ring, maps it into the current
//! process, and installs the anonymous-inode file.

use alloc::sync::Arc;

use kcred::Cred;
use kerrno::KResult;
use kvfs::VfsFile;

use crate::{context::AsyncContext, file, ring::Ring};

/// Result of `asyncio_setup` that the syscall layer copies back to userspace.
pub struct AsyncioSetup {
    /// Newly created async context file.
    pub file: Arc<VfsFile>,
    /// User virtual address of the mapped ring.
    pub ring_user_va: usize,
    /// Byte length of the mapped ring.
    pub ring_user_len: usize,
    /// Submission-ring entry count.
    pub sq_entries: u32,
    /// Completion-ring entry count.
    pub cq_entries: u32,
}

/// Creates one async context with an `entries`-deep submission ring.
///
/// The ring is mapped into the calling process's address space before the
/// descriptor is returned; the caller must be a sleepable user thread of
/// that process.
///
/// # Errors
///
/// Rejects non-power-of-two or oversized entry counts with `InvalidInput`,
/// and forwards address-space mapping, page-allocation, and anonymous-file
/// creation errors.
pub fn setup(entries: u32, cred: Arc<Cred>) -> KResult<AsyncioSetup> {
    let context = AsyncContext::new()?;
    let ring = Ring::create(entries)?;
    let ring_user_va = ring.user_base();
    let ring_user_len = ring.user_len();
    let sq_entries = ring.sq_entries();
    let cq_entries = ring.cq_entries();
    Ok(AsyncioSetup {
        file: file::new_file(context, ring, cred)?,
        ring_user_va,
        ring_user_len,
        sq_entries,
        cq_entries,
    })
}
