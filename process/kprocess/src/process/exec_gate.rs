// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-wide `exec` serialization.
//!
//! Two sibling threads that call `execve` at the same time each run a
//! `de_thread` sibling sweep that sends `SIGKILL` to the other and then blocks
//! in `join()`, so neither can finish: a mutual deadlock. Linux avoids it with
//! the process-wide `cred_guard_mutex`. This gate provides the same
//! serialization. The losing thread waits interruptibly and is killed by the
//! winner's sweep instead of joining it.

use alloc::sync::Arc;
use core::{
    future::poll_fn,
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};

use kpoll::{PollRegistrations, PollSet};
use ktask::{KtaskRef, future::block_on};

/// Serializes concurrent `exec` within one process.
pub(crate) struct ExecGate {
    in_progress: AtomicBool,
    released: Arc<PollSet>,
}

impl ExecGate {
    pub(crate) fn new() -> Self {
        Self {
            in_progress: AtomicBool::new(false),
            released: Arc::new(PollSet::new()),
        }
    }

    fn try_acquire(&self) -> bool {
        self.in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn release(&self) {
        self.in_progress.store(false, Ordering::Release);
        self.released.wake();
    }

    /// Acquires the gate, waiting interruptibly.
    ///
    /// Returns `false` when `current` was interrupted while waiting, which means
    /// the winner's sweep has a fatal signal pending for it; the caller must
    /// terminate rather than proceed with `exec`.
    fn acquire(&self, current: &KtaskRef) -> bool {
        if self.try_acquire() {
            return true;
        }
        let mut release_registrations = PollRegistrations::new();
        let mut interrupt_registrations = PollRegistrations::new();
        block_on(poll_fn(|cx| {
            if self.try_acquire() {
                return Poll::Ready(true);
            }
            let mut context = release_registrations.context(cx);
            if context.register(&self.released).is_err() {
                return Poll::Ready(false);
            }
            drop(context);
            if self.try_acquire() {
                return Poll::Ready(true);
            }
            let mut interrupt_context = interrupt_registrations.context(cx);
            match current.poll_interrupt(&mut interrupt_context) {
                Ok(Poll::Ready(())) => Poll::Ready(false),
                Ok(Poll::Pending) => Poll::Pending,
                Err(_) => Poll::Ready(false),
            }
        }))
    }
}

/// Releases the process `exec` gate on drop.
pub struct ExecGateGuard<'a> {
    gate: &'a ExecGate,
}

impl Drop for ExecGateGuard<'_> {
    fn drop(&mut self) {
        self.gate.release();
    }
}

impl super::Process {
    /// Acquires the process-wide `exec` gate, waiting interruptibly.
    ///
    /// Returns `None` when interrupted while waiting (a fatal signal such as the
    /// winning exec's sweep `SIGKILL`), in which case the caller must terminate
    /// instead of proceeding with `exec`.
    pub fn acquire_exec_gate(&self, current: &KtaskRef) -> Option<ExecGateGuard<'_>> {
        if self.exec_gate.acquire(current) {
            Some(ExecGateGuard {
                gate: &self.exec_gate,
            })
        } else {
            None
        }
    }
}
