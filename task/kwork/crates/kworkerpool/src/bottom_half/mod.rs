// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bottom-half worker-pool implementation.

mod candidate;
mod core;
mod pool;
mod slot;

pub use candidate::{
    BottomHalfRunnableCandidate, BottomHalfRunnableCandidateDiscard,
    BottomHalfRunnableCandidateResult,
};
pub use pool::BottomHalfWorkerPool;
