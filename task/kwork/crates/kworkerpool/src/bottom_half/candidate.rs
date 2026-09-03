// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bottom-half runnable candidate types.

use crate::{PoolEntry, WorkerExecutionToken};

/// Runnable candidate reserved by a bottom-half drain slot.
#[derive(Debug, Eq, PartialEq)]
pub struct BottomHalfRunnableCandidate {
    /// Popped runnable entry.
    pub entry: PoolEntry,
    /// Execution token reserved for this candidate.
    pub token: WorkerExecutionToken,
}

/// Result of trying to prepare one bottom-half runnable candidate.
#[derive(Debug, Eq, PartialEq)]
pub enum BottomHalfRunnableCandidateResult {
    /// A candidate was popped and reserved for external claim.
    Candidate(BottomHalfRunnableCandidate),
    /// No runnable entry is available for this drain pass.
    Empty,
}

/// Result of discarding a stale bottom-half runnable candidate.
#[derive(Debug, Eq, PartialEq)]
pub struct BottomHalfRunnableCandidateDiscard {
    /// Whether the drain context should immediately try another runnable entry.
    pub should_retry: bool,
}
