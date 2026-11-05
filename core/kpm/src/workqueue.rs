// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Caller-selected reversible queue participation, without a global registry.

use alloc::vec::Vec;

use ktime_types::TimeSpan;
use kworkng::{FrozenQueue, NormalQueue, QueueFreezeCancellation, QueueFreezeError};

/// Private normal queues whose callbacks must be stopped before device suspend.
///
/// Select dependent queues before providers; previously claimed callbacks must
/// finish without frozen tasks or pending work on an earlier paused queue.
/// Exempt their existing workers explicitly through [`crate::SuspendTasks`].
/// Keep PM, RTC and recovery dependencies off these queues. This selection does
/// not cover shared defaults, unlisted queues, BH/IRQ work or timer delivery.
pub struct SuspendWorkqueues<'a> {
    /// Explicit queue participation; clones of one queue share pause ownership.
    /// Repeating a queue fails Busy at its later position and rolls back the prefix.
    pub queues: &'a [NormalQueue],
    /// Maximum advancing-MONOTONIC wait for each queue's claimed callbacks.
    /// The total preparation budget is not limited to this duration.
    pub timeout: TimeSpan,
    /// One-shot queue-phase cancellation, prepared before entering PM.
    /// Also signal the task-phase handle when cancelling the whole preparation.
    pub cancellation: &'a QueueFreezeCancellation,
}

/// Failed queue preparation, retaining its position in the supplied list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkqueueFreezeError {
    /// Zero-based index of the first queue that could not freeze.
    pub index: usize,
    /// Underlying context, timer, closed, busy, cancellation or timeout failure.
    pub error: QueueFreezeError,
}

/// Owns one PM attempt's queue policy and acquired pause guards.
/// Successful guards survive a later failure until platform cleanup completes.
pub(crate) struct WorkqueueFreezeSession<'a> {
    policy: Option<SuspendWorkqueues<'a>>,
    frozen: Vec<FrozenQueue>,
}

impl<'a> WorkqueueFreezeSession<'a> {
    pub(crate) fn empty() -> Self {
        Self {
            policy: None,
            frozen: Vec::new(),
        }
    }

    pub(crate) fn new(policy: SuspendWorkqueues<'a>) -> Self {
        // Reserve before platform preparation and task freezing. Each queue
        // still prepares its own timeout notification while time is advancing.
        let frozen = Vec::with_capacity(policy.queues.len());
        Self {
            policy: Some(policy),
            frozen,
        }
    }

    pub(crate) fn freeze(&mut self) -> Result<(), WorkqueueFreezeError> {
        let Some(policy) = &self.policy else {
            return Ok(());
        };
        for (index, queue) in policy.queues.iter().enumerate() {
            let guard = queue
                .freeze(policy.timeout, policy.cancellation)
                .map_err(|error| WorkqueueFreezeError { index, error })?;
            self.frozen.push(guard);
        }
        // On failure the successful prefix remains owned until platform finish.
        Ok(())
    }

    pub(crate) fn thaw(&mut self) {
        while let Some(guard) = self.frozen.pop() {
            drop(guard);
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/workqueue.rs");
