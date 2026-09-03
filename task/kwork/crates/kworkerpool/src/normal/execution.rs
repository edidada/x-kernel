// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Encoding for ktask-local worker-pool execution identity.

use kcpu_id_map::LogicalCpuId;

use crate::{WorkerExecutionToken, WorkerId};

const CONTEXT_POOL_IDENTITY: usize = 0;
const CONTEXT_CPU_ID: usize = 1;
const CONTEXT_WORKER_ID: usize = 2;
const CONTEXT_TOKEN: usize = 3;

/// Opaque identity of one ktask-backed worker-pool instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkerPoolIdentity(usize);

impl WorkerPoolIdentity {
    /// Creates an identity from a stable runtime-owned value.
    pub(crate) const fn new(value: usize) -> Self {
        Self(value)
    }

    /// Returns the opaque identity value.
    pub(crate) const fn as_usize(self) -> usize {
        self.0
    }
}

/// Decoded worker-pool execution identity stored in a ktask.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolExecutionContext {
    pub(crate) pool: WorkerPoolIdentity,
    pub cpu_id: LogicalCpuId,
    pub(crate) worker: WorkerId,
    pub(crate) token: WorkerExecutionToken,
}

/// Encodes worker-pool execution identity into ktask's opaque task context.
pub(crate) fn encode_task_context(
    pool: WorkerPoolIdentity,
    cpu_id: LogicalCpuId,
    worker: WorkerId,
    token: WorkerExecutionToken,
) -> ktask::TaskExecutionContext {
    ktask::TaskExecutionContext::new([
        pool.as_usize(),
        cpu_id.as_usize(),
        worker.as_usize(),
        token.as_usize(),
    ])
}

/// Decodes ktask's opaque task context back into worker-pool identity.
pub fn decode_task_context(
    context: ktask::TaskExecutionContext,
) -> Option<WorkerPoolExecutionContext> {
    let words = context.words();
    Some(WorkerPoolExecutionContext {
        pool: WorkerPoolIdentity::new(words[CONTEXT_POOL_IDENTITY]),
        cpu_id: LogicalCpuId::new(words[CONTEXT_CPU_ID]),
        worker: WorkerId::new(words[CONTEXT_WORKER_ID]),
        token: WorkerExecutionToken::from_usize(words[CONTEXT_TOKEN]),
    })
}
