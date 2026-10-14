// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pending signal queues for processes and threads.
use alloc::{boxed::Box, collections::vec_deque::VecDeque};
use core::array;

use crate::{MAX_SIGNALS, SignalInfo, SignalSet, Signo};

/// Queue for managing pending signals awaiting delivery.
///
/// This structure maintains separate handling for standard signals (1-31)
/// and real-time signals (32-64), as they have different queuing semantics.
/// Standard signals can only have one pending instance, while real-time
/// signals can queue multiple instances.
pub struct PendingSignals {
    /// Bitmask of all pending signals
    pub set: SignalSet,

    /// Signal information for standard signals (1-31)
    /// Only one instance can be pending at a time
    info_std: [Option<Box<SignalInfo>>; 32],
    /// Signal information queues for real-time signals (32-64)
    /// Multiple instances can be queued
    info_rt: [VecDeque<SignalInfo>; MAX_SIGNALS - 31],
}

impl Default for PendingSignals {
    fn default() -> Self {
        Self {
            set: SignalSet::default(),
            info_std: Default::default(),
            info_rt: array::from_fn(|_| VecDeque::new()),
        }
    }
}

impl PendingSignals {
    /// Adds a signal to the pending queue.
    ///
    /// For standard signals (1-31), only one instance can be pending.
    /// For real-time signals (32-64), multiple instances can be queued.
    ///
    /// # Arguments
    /// * `sig` - Signal information to add
    ///
    /// # Returns
    /// `true` if the signal was successfully added, `false` if it was
    /// already pending (for standard signals only)
    pub fn put_signal(&mut self, sig: SignalInfo) -> bool {
        let signo = sig.signo();
        let added = self.set.add(signo);

        if signo.is_realtime() {
            self.info_rt[signo as usize - 32].push_back(sig);
        } else {
            if !added {
                // At most one standard signal can be pending. Prefer a
                // user-originated instance over a kernel-originated one, so
                // `exec_de_thread` can drop only its own kernel group kill
                // without losing a concurrent user `kill(2)`/`tgkill(2)`.
                let slot = &mut self.info_std[signo as usize];
                let pending_is_kernel = slot.as_ref().is_some_and(|pending| pending.code() > 0);
                if sig.code() <= 0 && pending_is_kernel {
                    *slot = Some(Box::new(sig));
                    return true;
                }
                return false;
            }
            self.info_std[signo as usize] = Some(Box::new(sig));
        }
        true
    }

    /// Dequeues the next pending signal contained in `mask`, if any.
    pub fn dequeue_signal(&mut self, mask: &SignalSet) -> Option<SignalInfo> {
        self.set.dequeue(mask).and_then(|signo| {
            if signo.is_realtime() {
                let queue = &mut self.info_rt[signo as usize - 32];
                let result = queue.pop_front();
                if !queue.is_empty() {
                    self.set.add(signo);
                }
                result
            } else {
                self.info_std[signo as usize].take().map(|boxed| *boxed)
            }
        })
    }

    /// Removes a pending standard signal only when its queued instance is
    /// kernel-originated (`si_code > 0`), leaving a user-originated instance
    /// queued. Returns whether an entry was removed.
    ///
    /// `exec` de_thread uses this to drop only the group-kill its sibling
    /// sweep produced, not a concurrent user `kill(2)`/`tgkill(2)`.
    pub fn discard_kernel_standard(&mut self, signo: Signo) -> bool {
        if signo.is_realtime() {
            return false;
        }
        let slot = &mut self.info_std[signo as usize];
        if slot.as_ref().is_some_and(|sig| sig.code() > 0) {
            slot.take();
            self.set.remove(signo);
            return true;
        }
        false
    }
}
