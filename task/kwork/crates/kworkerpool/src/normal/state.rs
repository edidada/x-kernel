// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ordinary task-backed worker-pool transition results.

use crate::{ActionBatch, PoolEntry, WorkerExecutionToken};

/// Runnable candidate reserved for lock-free external claim validation.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnableCandidate {
    /// Popped runnable entry.
    pub entry: PoolEntry,
    /// Execution token reserved for this candidate.
    pub token: WorkerExecutionToken,
    /// Pool actions produced while dispatching.
    pub actions: ActionBatch,
}

/// Result of trying to prepare one runnable candidate.
#[derive(Debug, Eq, PartialEq)]
pub enum RunnableCandidateResult {
    /// A candidate was popped and reserved for external claim.
    Candidate(RunnableCandidate),
    /// No runnable entry is available for this worker.
    Empty(ActionBatch),
}

/// Result of discarding a stale runnable candidate.
#[derive(Debug, Eq, PartialEq)]
pub struct RunnableCandidateDiscard {
    /// Whether the worker should immediately try another runnable entry.
    pub should_retry: bool,
    /// Pool actions produced while discarding the candidate.
    pub actions: ActionBatch,
}
