// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux `io_uring` shared-ring memory (single-mmap subset).
//!
//! The kernel allocates one physically contiguous 2 MiB block and keeps a
//! direct-map pointer for all ring accesses. Userspace obtains the same pages
//! through `mmap(fd, IORING_OFF_SQ_RING)` and `mmap(fd, IORING_OFF_SQES)`.
//!
//! Layout inside the block:
//!
//! ```text
//! 0x0000  SQ head/tail/mask/entries/flags/dropped
//! 0x0040  u32 SQ index array (sq_entries entries)
//! align   CQ head/tail/mask/entries/overflow
//! align   struct io_uring_cqe array (cq_entries entries)
//! align   struct io_uring_sqe array (sq_entries entries)
//! ```
//!
//! Index storage is shared memory and therefore hostile: the kernel keeps its
//! own `sq_head`/`cq_tail`, masks every user-published index, and validates
//! SQEs before queueing them.

use alloc::sync::Arc;
use core::{
    ptr,
    sync::atomic::{Ordering, fence},
};

use kerrno::{KError, KResult};
use khal::{mem::p2v, paging::PageSize};
use memaddr::{PhysAddr, align_up_4k};
use memspace::backend::SharedPages;

use crate::{
    abi::{
        IO_URING_CQE_SIZE, IO_URING_SQE_SIZE, IORING_FEAT_SINGLE_MMAP, IoUringCqOffsets,
        IoUringCqe, IoUringSqOffsets, IoUringSqe,
    },
    context::{AsyncContext, PreparedOp},
};

/// Backing block size. Large enough for 16384 SQEs plus the CQ ring.
pub(crate) const RING_MEM_SIZE: usize = 2 * 1024 * 1024;
/// Maximum supported SQ/CQ depth for this subset.
pub(crate) const MAX_ENTRIES: u32 = 16_384;

const SQ_HEAD: usize = 0x00;
const SQ_TAIL: usize = 0x04;
const SQ_MASK: usize = 0x08;
const SQ_ENTRIES: usize = 0x0c;
const SQ_FLAGS: usize = 0x10;
const SQ_DROPPED: usize = 0x14;
const SQ_ARRAY: usize = 0x40;

const CQ_HEADER_SIZE: usize = 0x40;

/// Kernel-side state and layout of one io_uring ring.
pub(crate) struct Ring {
    _pages: Arc<SharedPages>,
    base: *mut u8,
    base_paddr: PhysAddr,
    sq_entries: u32,
    cq_entries: u32,
    sq_array_off: usize,
    cq_head_off: usize,
    cqes_off: usize,
    sqes_off: usize,
    ring_len: usize,
    /// Kernel-consumed SQ index (free-running).
    sq_head: u32,
    /// Kernel-published CQ index (free-running).
    cq_tail: u32,
}

// SAFETY: the direct-map pointer is stable for the lifetime of `pages`; all
// accesses are volatile and synchronized through the shared head/tail words.
unsafe impl Send for Ring {}
// SAFETY: see `Send`; shared accesses are volatile and index-masked.
unsafe impl Sync for Ring {}

impl Ring {
    /// Allocates and initializes ring memory for `entries` SQEs.
    ///
    /// Requires sleepable kernel context with the direct map available.
    pub(crate) fn create(entries: u32) -> KResult<Self> {
        if entries == 0 || entries > MAX_ENTRIES {
            return Err(KError::InvalidInput);
        }
        // Linux rounds the requested depth up to a power of two.
        let entries = entries.next_power_of_two();
        let cq_entries = entries;
        let sq_array_off = SQ_ARRAY;
        let sq_array_end = sq_array_off + entries as usize * 4;
        let cq_head_off = align_up_4k(sq_array_end);
        let cqes_off = align_up_4k(cq_head_off + CQ_HEADER_SIZE);
        let sqes_off = align_up_4k(cqes_off + cq_entries as usize * IO_URING_CQE_SIZE);
        let used = sqes_off + entries as usize * IO_URING_SQE_SIZE;
        if used > RING_MEM_SIZE {
            return Err(KError::InvalidInput);
        }

        let pages = Arc::new(SharedPages::new(RING_MEM_SIZE, PageSize::Size2M)?);
        let base_paddr = *pages.first().ok_or(KError::NoMemory)?;
        let base = p2v(base_paddr).as_mut_ptr();

        let ring = Self {
            _pages: pages,
            base,
            base_paddr,
            sq_entries: entries,
            cq_entries,
            sq_array_off,
            cq_head_off,
            cqes_off,
            sqes_off,
            ring_len: RING_MEM_SIZE,
            sq_head: 0,
            cq_tail: 0,
        };
        ring.write_u32(SQ_HEAD, 0);
        ring.write_u32(SQ_TAIL, 0);
        ring.write_u32(SQ_MASK, entries - 1);
        ring.write_u32(SQ_ENTRIES, entries);
        ring.write_u32(SQ_FLAGS, 0);
        ring.write_u32(SQ_DROPPED, 0);
        ring.write_u32(cq_head_off, 0);
        ring.write_u32(cq_head_off + 4, 0);
        ring.write_u32(cq_head_off + 8, cq_entries - 1);
        ring.write_u32(cq_head_off + 12, cq_entries);
        ring.write_u32(cq_head_off + 16, 0);
        Ok(ring)
    }

    /// Returns the physical base address of the ring block.
    pub(crate) fn base_paddr(&self) -> PhysAddr {
        self.base_paddr
    }

    /// Returns the complete mapped block length.
    pub(crate) fn ring_len(&self) -> usize {
        self.ring_len
    }

    /// Returns the offset of the SQE array inside the block.
    pub(crate) fn sqes_offset(&self) -> usize {
        self.sqes_off
    }

    /// Returns the SQ depth.
    pub(crate) fn sq_entries(&self) -> u32 {
        self.sq_entries
    }

    /// Returns the CQ depth.
    pub(crate) fn cq_entries(&self) -> u32 {
        self.cq_entries
    }

    /// Returns the `io_sqring_offsets` reported in `io_uring_params`.
    pub(crate) fn sq_offsets(&self) -> IoUringSqOffsets {
        IoUringSqOffsets {
            head: SQ_HEAD as u32,
            tail: SQ_TAIL as u32,
            ring_mask: SQ_MASK as u32,
            ring_entries: SQ_ENTRIES as u32,
            flags: SQ_FLAGS as u32,
            dropped: SQ_DROPPED as u32,
            array: self.sq_array_off as u32,
            resv1: 0,
            user_addr: 0,
        }
    }

    /// Returns the `io_cqring_offsets` reported in `io_uring_params`.
    pub(crate) fn cq_offsets(&self) -> IoUringCqOffsets {
        IoUringCqOffsets {
            head: self.cq_head_off as u32,
            tail: (self.cq_head_off + 4) as u32,
            ring_mask: (self.cq_head_off + 8) as u32,
            ring_entries: (self.cq_head_off + 12) as u32,
            overflow: (self.cq_head_off + 16) as u32,
            cqes: self.cqes_off as u32,
            flags: 0,
            resv1: 0,
            user_addr: 0,
        }
    }

    /// Returns the feature bits this ring implements.
    pub(crate) fn features(&self) -> u32 {
        IORING_FEAT_SINGLE_MMAP
    }

    /// Consumes up to `to_submit` SQEs from the shared SQ ring.
    pub(crate) fn submit(&mut self, ctx: &AsyncContext, to_submit: u32) -> KResult<u32> {
        if to_submit > self.sq_entries {
            return Err(KError::InvalidInput);
        }
        let tail = self.read_u32(SQ_TAIL);
        let available = tail.wrapping_sub(self.sq_head);
        if to_submit > available {
            return Err(KError::InvalidInput);
        }
        fence(Ordering::Acquire);

        let mask = self.sq_entries - 1;
        let mut submitted = 0;
        for index in 0..to_submit {
            let sq_index = (self.sq_head.wrapping_add(index) & mask) as usize;
            let array_off = self.sq_array_off + sq_index * 4;
            let sqe_index = self.read_u32(array_off);
            if sqe_index >= self.sq_entries {
                if submitted == 0 {
                    return Err(KError::InvalidInput);
                }
                break;
            }
            let sqe = self.read_sqe(sqe_index);
            if sqe.flags != 0 || !sqe.is_supported_opcode() {
                if submitted == 0 {
                    return Err(KError::InvalidInput);
                }
                break;
            }
            match ctx.push_ready(PreparedOp {
                user_data: sqe.user_data,
            }) {
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
        self.write_u32(SQ_HEAD, self.sq_head);
        Ok(submitted)
    }

    /// Publishes staged completions into the shared CQ ring.
    pub(crate) fn flush_completions(&mut self, ctx: &AsyncContext) -> KResult<u32> {
        let mut published = 0;
        loop {
            let cq_head = self.read_u32(self.cq_head_off);
            if self.cq_tail.wrapping_sub(cq_head) >= self.cq_entries {
                break;
            }
            let Some(completion) = ctx.peek_completion() else {
                break;
            };
            let slot = (self.cq_tail & (self.cq_entries - 1)) as usize;
            let cqe = IoUringCqe {
                user_data: completion.user_data,
                res: completion.res,
                flags: 0,
            };
            self.write_cqe(slot, cqe);
            let _ = ctx.pop_completion();
            fence(Ordering::Release);
            self.cq_tail = self.cq_tail.wrapping_add(1);
            self.write_u32(self.cq_head_off + 4, self.cq_tail);
            published += 1;
        }
        Ok(published)
    }

    /// Returns whether the completion ring is full.
    pub(crate) fn is_cq_full(&self) -> bool {
        let cq_head = self.read_u32(self.cq_head_off);
        self.cq_tail.wrapping_sub(cq_head) >= self.cq_entries
    }

    fn read_u32(&self, offset: usize) -> u32 {
        let ptr = self.base.wrapping_add(offset).cast::<u32>();
        // SAFETY: `offset` is inside the 2 MiB ring block and the header field
        // is aligned and initialized before publication.
        unsafe { ptr::read_volatile(ptr) }
    }

    fn write_u32(&self, offset: usize, value: u32) {
        let ptr = self.base.wrapping_add(offset).cast::<u32>();
        // SAFETY: see `read_u32`; the header field is kernel-owned.
        unsafe { ptr::write_volatile(ptr, value) }
    }

    fn read_sqe(&self, index: u32) -> IoUringSqe {
        let offset = self.sqes_off + index as usize * IO_URING_SQE_SIZE;
        let ptr = self.base.wrapping_add(offset).cast::<IoUringSqe>();
        // SAFETY: the SQE array is inside the ring block and `index` is
        // bounded by `sq_entries`; the source is aligned user-shared memory.
        unsafe { ptr::read_volatile(ptr) }
    }

    fn write_cqe(&self, slot: usize, cqe: IoUringCqe) {
        let offset = self.cqes_off + slot * IO_URING_CQE_SIZE;
        let ptr = self.base.wrapping_add(offset).cast::<IoUringCqe>();
        // SAFETY: the CQE array is inside the ring block and `slot` is masked
        // to `cq_entries`; the destination is aligned user-shared memory.
        unsafe { ptr::write_volatile(ptr, cqe) }
    }
}
