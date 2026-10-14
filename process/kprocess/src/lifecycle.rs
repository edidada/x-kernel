// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lifecycle state shared by all threads in a process.

use alloc::sync::Arc;

use kpoll::{Completion, PollSet};

/// Process lifecycle state shared by all threads in a process.
pub(crate) struct ProcessLifecycleState {
    events: ProcessEvents,
}

struct ProcessEvents {
    child_exit_event: Arc<PollSet>,
    exit_event: Arc<Completion>,
}

impl Default for ProcessLifecycleState {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessLifecycleState {
    /// Creates a new [`ProcessLifecycleState`].
    pub(crate) fn new() -> Self {
        Self {
            events: ProcessEvents::new(),
        }
    }

    /// Returns the child-exit wait event.
    pub(crate) fn child_exit_event(&self) -> &Arc<PollSet> {
        self.events.child_exit_event()
    }

    /// Wakes waiters that are blocked on a child process becoming waitable.
    pub(crate) fn notify_child_exit(&self) {
        self.events.notify_child_exit();
    }

    /// Returns the process-exit event.
    pub(crate) fn exit_event(&self) -> &Arc<Completion> {
        self.events.exit_event()
    }

    /// Wakes waiters that are blocked on this process exiting.
    pub(crate) fn notify_exit(&self) {
        self.events.notify_exit();
    }
}

impl ProcessEvents {
    fn new() -> Self {
        Self {
            child_exit_event: Arc::default(),
            exit_event: Arc::default(),
        }
    }

    fn child_exit_event(&self) -> &Arc<PollSet> {
        &self.child_exit_event
    }

    fn notify_child_exit(&self) {
        self.child_exit_event.wake();
    }

    fn exit_event(&self) -> &Arc<Completion> {
        &self.exit_event
    }

    fn notify_exit(&self) {
        self.exit_event.complete_all();
    }
}
