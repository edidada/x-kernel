// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User/kernel shared submission and completion ring.
//!
//! The ring lives in anonymous shared pages mapped into the submitting
//! process. The kernel keeps its own `sq_head`/`cq_tail` cursors in
//! [`Ring`]; user-owned `sq_tail`/`cq_head` are read from the shared header
//! on every operation. All field accesses are aligned 32-bit copies, and
//! fences separate SQE/CQE payload publication from the index update.

use alloc::sync::Arc;
use core::sync::atomic::{Ordering, fence};

use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize};
use memaddr::{PAGE_SIZE_4K, VirtAddr, align_up_4k};
use memspace::{AddrPolicy, VmRuntimeRef, backend::SharedPages};
use posix_types::UserPtr;

use crate::{
    abi::{ASYNCIO_CQE_SIZE, ASYNCIO_MAX_SQ_ENTRIES, ASYNCIO_SQE_SIZE, AsyncCqe, AsyncSqe, header},
    context::{AsyncContext, PreparedOp},
};

/// Kernel-owned state of one async context's ring.
pub(crate) struct Ring {
    user_base: usize,
    user_len: usize,
    sq_entries: u32,
    cq_entries: u32,
    /// Kernel-consumed submission index (free-running).
    sq_head: u32,
    /// Kernel-published completion index (free-running).
    cq_tail: u32,
}

impl Ring {
    /// Allocates ring pages, maps them into the current process, and
    /// initializes the shared header.
    ///
    /// Requires syscall context with the current process's live address
    /// space; the returned base is a user VA owned by that process.
    pub(crate) fn create(entries: u32) -> KResult<Self> {
        if entries == 0 || !entries.is_power_of_two() || entries > ASYNCIO_MAX_SQ_ENTRIES {
            return Err(KError::InvalidInput);
        }
        let cq_entries = entries;
        let sqe_bytes = entries as usize * ASYNCIO_SQE_SIZE;
        let cqe_bytes = cq_entries as usize * ASYNCIO_CQE_SIZE;
        let user_len = align_up_4k(header::LEN + sqe_bytes + cqe_bytes);

        let pages = Arc::new(SharedPages::new(user_len, PageSize::Size4K)?);
        let object = anon::AnonSharedObject::new();
        let process = kprocess::current_user_thread().process().clone();
        let address_space = process.address_space()?;
        let user_base = address_space.with_mapping_owner(|mut mapping| {
            let start = mapping.aspace_mut().mmap_resolve_addr(
                VirtAddr::from(0),
                user_len,
                PAGE_SIZE_4K,
                AddrPolicy::Any,
            )?;
            mapping.aspace_mut().map_with_max_flags(
                start,
                user_len,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                true,
                VmRuntimeRef::new_shared_pages(start, pages, object),
            )?;
            Ok::<_, KError>(start.as_usize())
        })?;

        let ring = Self {
            user_base,
            user_len,
            sq_entries: entries,
            cq_entries,
            sq_head: 0,
            cq_tail: 0,
        };
        ring.write_field(header::SQ_HEAD, 0)?;
        ring.write_field(header::SQ_TAIL, 0)?;
        ring.write_field(header::SQ_MASK, entries - 1)?;
        ring.write_field(header::SQ_ENTRIES, entries)?;
        ring.write_field(header::CQ_HEAD, 0)?;
        ring.write_field(header::CQ_TAIL, 0)?;
        ring.write_field(header::CQ_MASK, cq_entries - 1)?;
        ring.write_field(header::CQ_ENTRIES, cq_entries)?;
        ring.write_field(header::CTX_FLAGS, 0)?;
        Ok(ring)
    }

    /// Returns the user VA of the ring mapping.
    pub(crate) fn user_base(&self) -> usize {
        self.user_base
    }

    /// Returns the mapped ring length in bytes.
    pub(crate) fn user_len(&self) -> usize {
        self.user_len
    }

    /// Returns the submission-ring entry count.
    pub(crate) fn sq_entries(&self) -> u32 {
        self.sq_entries
    }

    /// Returns the completion-ring entry count.
    pub(crate) fn cq_entries(&self) -> u32 {
        self.cq_entries
    }

    /// Consumes up to `to_submit` published SQEs from the shared ring and
    /// queues their operations.
    ///
    /// Entries are validated before acceptance. A queue-full or shutdown
    /// rejection stops the batch at the first rejected entry and the kernel
    /// head advances only over accepted entries, so the caller can retry the
    /// remainder on the next `enter`. Returns the number submitted.
    ///
    /// # Errors
    ///
    /// Rejects an oversized batch or a count beyond the user-published tail
    /// with `InvalidInput`, and an invalid first SQE with `InvalidInput`.
    /// Forwards a shutdown rejection when no entry was accepted.
    pub(crate) fn submit(&mut self, ctx: &AsyncContext, to_submit: u32) -> KResult<u32> {
        if to_submit > self.sq_entries {
            return Err(KError::InvalidInput);
        }
        let tail = self.read_field(header::SQ_TAIL)?;
        let available = tail.wrapping_sub(self.sq_head);
        if to_submit > available {
            return Err(KError::InvalidInput);
        }

        // User publishes the SQE payloads before advancing the tail.
        fence(Ordering::Acquire);

        let mask = self.sq_entries - 1;
        let mut submitted = 0;
        for index in 0..to_submit {
            let slot = self.sq_head.wrapping_add(index) & mask;
            let offset = header::LEN + slot as usize * ASYNCIO_SQE_SIZE;
            let sqe: AsyncSqe = UserPtr::from(self.user_base + offset).read_vm()?;
            if !sqe.is_abi_clean() || !sqe.is_known_opcode() {
                if submitted == 0 {
                    return Err(KError::InvalidInput);
                }
                break;
            }
            match ctx.push_ready(PreparedOp { cookie: sqe.cookie }) {
                Ok(()) => submitted += 1,
                Err(KError::WouldBlock) => break,
                Err(error) => {
                    if submitted == 0 {
                        return Err(error);
                    }
                    break;
                }
            }
        }
        self.sq_head = self.sq_head.wrapping_add(submitted);
        self.write_field(header::SQ_HEAD, self.sq_head)?;
        Ok(submitted)
    }

    /// Publishes staged completions into the shared completion ring.
    ///
    /// Stops when the kernel staging queue is empty or the user has not
    /// consumed enough CQEs to make room. Returns the number published.
    pub(crate) fn flush_completions(&mut self, ctx: &AsyncContext) -> KResult<u32> {
        let mut published = 0;
        loop {
            let cq_head = self.read_field(header::CQ_HEAD)?;
            if self.cq_tail.wrapping_sub(cq_head) >= self.cq_entries {
                break;
            }
            let Some(completion) = ctx.peek_completion() else {
                break;
            };
            let slot = self.cq_tail & (self.cq_entries - 1);
            let offset = header::LEN
                + self.sq_entries as usize * ASYNCIO_SQE_SIZE
                + slot as usize * ASYNCIO_CQE_SIZE;
            let cqe = AsyncCqe {
                cookie: completion.cookie,
                res: completion.res,
                reserved0: 0,
                flags: 0,
                abi_pad: 0,
            };
            UserPtr::from(self.user_base + offset).write_vm(cqe)?;
            // Only consume the staged completion after the CQE is in user
            // memory, so a faulting ring access cannot lose the cookie.
            let _ = ctx.pop_completion();
            // CQE payload must be visible before the user observes the new tail.
            fence(Ordering::Release);
            self.cq_tail = self.cq_tail.wrapping_add(1);
            self.write_field(header::CQ_TAIL, self.cq_tail)?;
            published += 1;
        }
        Ok(published)
    }

    /// Publishes the shutdown flag in the shared header.
    pub(crate) fn mark_shutdown(&self) {
        let _ = self.write_field(header::CTX_FLAGS, header::CTX_FLAG_SHUTDOWN);
    }

    /// Returns whether the completion ring has no room for another CQE.
    pub(crate) fn is_cq_full(&self) -> KResult<bool> {
        let cq_head = self.read_field(header::CQ_HEAD)?;
        Ok(self.cq_tail.wrapping_sub(cq_head) >= self.cq_entries)
    }

    fn field(&self, offset: usize) -> UserPtr<u32> {
        UserPtr::from(self.user_base + offset)
    }

    fn read_field(&self, offset: usize) -> KResult<u32> {
        self.field(offset).read_vm().map_err(Into::into)
    }

    fn write_field(&self, offset: usize, value: u32) -> KResult<()> {
        self.field(offset).write_vm(value).map_err(Into::into)
    }
}
