// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host-side services for the MMC subsystem (the `subsys-block-host`
//! shape): the task-context [`XferSignal`] implementation backed by
//! kpoll/ktask.
//!
//! The contract lives host-free in `subsys-mmc` (L0-b); this crate is the
//! service half, which is what lets host drivers park a block worker on a
//! data transfer while mock domains run the protocol core without any
//! scheduler at all.

#![no_std]

extern crate alloc;

use kerrno::{KError, KResult};
use kpoll::PollSet;
use ktask::future::PreparedTaskWait;
/// Re-export so drivers name one subsystem family, not two crates.
pub use subsys_mmc::XferSignal;

/// The task-context [`XferSignal`] implementation (Linux
/// `wait_for_completion` behind a poll set).
///
/// Same register-before-check/park loop as the virtio-rng and
/// block-host waiters: a wake between check and park cannot be lost, and
/// spurious wakes recheck the readiness closure.
pub struct TaskXferSignal {
    waiters: PollSet,
}

impl TaskXferSignal {
    /// A fresh, empty signal.
    pub fn new() -> Self {
        Self {
            waiters: PollSet::new(),
        }
    }

    /// A shared handle for handing to host data structures.
    pub fn shared() -> alloc::sync::Arc<Self> {
        alloc::sync::Arc::new(Self::new())
    }
}

impl Default for TaskXferSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl XferSignal for TaskXferSignal {
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> KResult {
        let mut task_wait = PreparedTaskWait::prepare().map_err(|_| KError::InvalidInput)?;
        if ready() {
            return Ok(());
        }

        let waker = task_wait.waker();
        loop {
            // Register before rechecking so a wake between check and park
            // cannot be lost; retry after spurious wakes.
            let registration = self
                .waiters
                .register(&waker)
                .map_err(|_| KError::InvalidInput)?;
            if ready() {
                drop(registration);
                return Ok(());
            }
            task_wait.park();
            drop(registration);
        }
    }

    fn notify(&self) {
        self.waiters.wake();
    }
}
