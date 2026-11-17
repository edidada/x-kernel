// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded data transmission and its capacity wait source.

use alloc::collections::VecDeque;

use kpoll::{PollContext, PollRegisterError, PollSet};

use crate::{buf::PacketBuf, stack::device::TsoSegments};

/// Owns queued packets and capacity registrations under the enclosing I/O lock.
pub(crate) struct DataTxQueue {
    packets: VecDeque<PacketBuf>,
    capacity: usize,
    pending_tso: Option<TsoSegments>,
    capacity_poll: PollSet,
}

impl DataTxQueue {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            packets: VecDeque::with_capacity(capacity),
            capacity,
            pending_tso: None,
            capacity_poll: PollSet::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.packets.is_empty() && self.pending_tso.is_none()
    }

    pub(crate) fn is_full(&self) -> bool {
        self.pending_tso.is_some() || self.packets.len() >= self.capacity
    }

    pub(crate) fn can_push(&self, packet_count: usize) -> bool {
        packet_count <= self.available()
    }

    pub(crate) fn available(&self) -> usize {
        if self.pending_tso.is_some() {
            return 0;
        }
        self.capacity.saturating_sub(self.packets.len())
    }

    pub(crate) fn try_push(&mut self, packet: PacketBuf) -> Result<(), PacketBuf> {
        if self.is_full() {
            return Err(packet);
        }

        self.packets.push_back(packet);
        Ok(())
    }

    /// Retains one admitted batch and blocks producers until its last segment leaves.
    pub(crate) fn push_tso(&mut self, segments: TsoSegments) {
        assert!(!self.is_full(), "TSO token requires queue capacity");
        self.pending_tso = Some(segments);
    }

    /// Returns the unadvanced iterator when the complete batch does not fit.
    pub(crate) fn try_extend<I>(&mut self, packets: I) -> Result<(), I>
    where
        I: ExactSizeIterator<Item = PacketBuf>,
    {
        if !self.can_push(packets.len()) {
            return Err(packets);
        }

        self.packets.extend(packets);
        Ok(())
    }

    /// Removes a packet; the caller must notify capacity waiters after unlocking.
    pub(crate) fn pop_front(&mut self) -> Option<PacketBuf> {
        if let Some(packet) = self.packets.pop_front() {
            return Some(packet);
        }
        let segments = self.pending_tso.as_mut()?;
        let packet = segments.pop_front();
        if segments.is_empty() {
            self.pending_tso = None;
        }
        Some(packet)
    }

    pub(crate) fn register_capacity_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.capacity_poll)
    }

    /// Captures the queue's wait source for waking after the enclosing lock is released.
    ///
    /// Call only after removing data packets, including packets retained in a
    /// separate dispatch batch. Device transmission completion is not required.
    pub(crate) fn capacity_poll(&self) -> PollSet {
        self.capacity_poll.clone()
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/runtime/tx_queue/tests.rs");
