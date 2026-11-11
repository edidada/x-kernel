// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Anonymous-inode file object backing one async context.
//!
//! The file owns the [`AsyncContext`] and the shared ring cursors. Poll
//! readiness reports staged completions, and the operation-table `release`
//! callback shuts the context down when the last file reference goes away.

use alloc::sync::Arc;

use anon_inodefs::AnonInodeFs;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError};
use ksync::Mutex;
use ktask::future::{block_on, poll_io, timeout};
use ktime_types::TimeSpan;
use kvfs::{FMode, FileOperations, MmapMapper, OpenFlags, VfsFile, VfsResult};
use memaddr::PhysAddrRange;

use crate::{
    abi::{
        IORING_OFF_CQ_RING, IORING_OFF_SQ_RING, IORING_OFF_SQES, IoUringCqOffsets, IoUringSqOffsets,
    },
    context::{AsyncContext, CompletionSource},
    ring::Ring,
    worker::ensure_worker,
};

/// One async context exposed as a file descriptor.
pub struct AsyncioFile {
    context: Arc<AsyncContext>,
    pub(crate) ring: Mutex<Ring>,
}

impl AsyncioFile {
    /// Wraps a freshly created context and ring.
    pub(crate) fn new(context: Arc<AsyncContext>, ring: Ring) -> Self {
        Self {
            context,
            ring: Mutex::new(ring),
        }
    }

    /// Returns the async file attached to an `io_uring_setup` descriptor.
    ///
    /// # Errors
    ///
    /// Returns `BadFileDescriptor` when `file` is not an async descriptor.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    /// Submits SQEs, grows the worker pool, and optionally waits for
    /// completions to be published into the CQ ring.
    ///
    /// Returns the number of operations accepted from this call. A full
    /// completion ring ends the wait early without losing staged completions;
    /// the next `enter` publishes them.
    ///
    /// # Errors
    ///
    /// Rejects `min_complete` above the completion-ring size with
    /// `InvalidInput`, and forwards SQE validation, enqueue, worker-spawn,
    /// ring-access, and wait errors.
    pub fn enter(
        &self,
        to_submit: u32,
        min_complete: u32,
        timeout: Option<TimeSpan>,
    ) -> KResult<u32> {
        if min_complete > self.ring.lock().cq_entries() {
            return Err(KError::InvalidInput);
        }
        let submitted = self.ring.lock().submit(&self.context, to_submit)?;
        // Also cover work left queued by an earlier failed spawn: the growth
        // check is a no-op when the queue does not exceed idle capacity.
        ensure_worker(&self.context)?;
        if min_complete == 0 {
            self.ring.lock().flush_completions(&self.context)?;
        } else {
            self.wait_for_completions(min_complete, timeout)?;
        }
        Ok(submitted)
    }

    fn wait_for_completions(&self, min_complete: u32, time_limit: Option<TimeSpan>) -> KResult<()> {
        let deadline = time_limit.map(|span| khal::time::monotonic_time() + span);
        let mut published = 0;
        loop {
            published += self.ring.lock().flush_completions(&self.context)?;
            if published >= min_complete {
                return Ok(());
            }
            if self.ring.lock().is_cq_full() {
                // The user must consume CQEs before more can be published;
                // returning now is preferable to spinning in the kernel.
                return Ok(());
            }
            if self.context.is_shutdown() && !self.context.has_completions() {
                return Err(KError::BadFileDescriptor);
            }
            let remaining = match deadline {
                Some(deadline) => {
                    let remaining =
                        deadline.saturating_duration_since(khal::time::monotonic_time());
                    if remaining.is_zero() {
                        return Ok(());
                    }
                    Some(remaining)
                }
                None => None,
            };
            match block_on(timeout(
                remaining,
                poll_io(
                    &CompletionSource(&self.context),
                    IoEvents::IN,
                    false,
                    || {
                        if self.context.completion_readiness().contains(IoEvents::IN) {
                            Ok(())
                        } else {
                            Err(KError::WouldBlock)
                        }
                    },
                ),
            )) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => return Ok(()),
            }
        }
    }

    fn readiness(&self) -> IoEvents {
        self.context.completion_readiness()
    }

    fn register(&self, context: &mut PollContext<'_>) -> Result<(), PollRegisterError> {
        self.context.register_completion_wait(context)
    }

    /// Returns the SQ depth granted by the ring.
    pub fn sq_entries(&self) -> u32 {
        self.ring.lock().sq_entries()
    }

    /// Returns the CQ depth granted by the ring.
    pub fn cq_entries(&self) -> u32 {
        self.ring.lock().cq_entries()
    }

    /// Returns the `io_sqring_offsets` for `io_uring_params`.
    pub fn sq_offsets(&self) -> IoUringSqOffsets {
        self.ring.lock().sq_offsets()
    }

    /// Returns the `io_cqring_offsets` for `io_uring_params`.
    pub fn cq_offsets(&self) -> IoUringCqOffsets {
        self.ring.lock().cq_offsets()
    }

    /// Returns the advertised feature bits.
    pub fn features(&self) -> u32 {
        self.ring.lock().features()
    }
}

/// File operations for an async context descriptor.
pub(crate) struct AsyncioFops;

impl FileOperations for AsyncioFops {
    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        Some(
            Self::file(file)
                .map(|file| file.readiness())
                .unwrap_or_else(|_| IoEvents::empty()),
        )
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN)
            && let Ok(file) = Self::file(file)
        {
            file.register(context)?;
        }
        Ok(())
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        let file = Self::file(file)?;
        let ring = file.ring.lock();
        let offset = mapper.offset();
        let (start, len) = match offset {
            IORING_OFF_SQ_RING | IORING_OFF_CQ_RING => (ring.base_paddr(), ring.ring_len()),
            IORING_OFF_SQES => (
                memaddr::PhysAddr::from_usize(ring.base_paddr().as_usize() + ring.sqes_offset()),
                ring.ring_len() - ring.sqes_offset(),
            ),
            _ => return Err(kvfs::VfsError::InvalidInput),
        };
        mapper.map_physical(PhysAddrRange::from_start_size(start, len))
    }

    fn release(&self, _inode: &kvfs::VfsInode, file: &VfsFile) -> VfsResult<()> {
        if let Ok(file) = Self::file(file) {
            file.context.shutdown();
        }
        Ok(())
    }
}

impl AsyncioFops {
    fn file(file: &VfsFile) -> KResult<Arc<AsyncioFile>> {
        AsyncioFile::from_file(file)
    }
}

/// Creates the anonymous-inode file for a new async context.
pub(crate) fn new_file(
    context: Arc<AsyncContext>,
    ring: Ring,
    cred: Arc<kcred::Cred>,
) -> KResult<Arc<VfsFile>> {
    let state = Arc::new(AsyncioFile::new(context, ring));
    AnonInodeFs::global().get_file(
        "[io_uring]",
        Arc::new(AsyncioFops),
        state,
        FMode::READ | FMode::WRITE,
        OpenFlags::READ_WRITE,
        cred,
    )
}
