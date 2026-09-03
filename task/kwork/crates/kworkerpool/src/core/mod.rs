// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared worker-pool building blocks.

mod entry;
mod id;
pub(crate) mod queue;
mod state;

pub use entry::{EntryKey, EntryOwner, EntryPayload, EntrySource};
pub use id::WorkerId;
pub(crate) use queue::PoolRunQueue;
pub use queue::{PoolEntry, QueueRemoveResult};
pub use state::{
    ParkDecision, WorkerExecutionToken, WorkerPoolError, WorkerPoolSnapshot, WorkerState,
};
