// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Parent suspension until a vfork child stops accessing its old MM.

use alloc::sync::Arc;

use kcompletion::Completion;
use kerrno::KResult;
use ktask::completion::CompletionWait;

pub(super) struct VforkWait {
    completion: Arc<Completion>,
    task_wait: CompletionWait,
}

impl VforkWait {
    /// Prepares all wait storage before the child can become runnable.
    pub(super) fn prepare() -> KResult<Self> {
        let task_wait = CompletionWait::prepare()?;
        Ok(Self {
            completion: Arc::new(Completion::new()),
            task_wait,
        })
    }

    pub(super) fn completion(&self) -> Arc<Completion> {
        self.completion.clone()
    }

    /// Commits SIGKILL through the normal signal-exit transition after unlinking
    /// the wait. Shared pending state may already have been dequeued by a sibling;
    /// relying on a later signal scan could resume the parent's shared stack.
    /// Signal exit also preserves the exec de-thread sweep exemption.
    pub(super) fn wait(self) {
        if self.task_wait.wait_killable(&self.completion).is_err() {
            posix_process::do_exit_from_signal(ksignal::Signo::SIGKILL as i32);
        }
    }
}
