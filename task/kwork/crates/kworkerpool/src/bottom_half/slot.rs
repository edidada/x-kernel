// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bottom-half drain slot state.

use crate::{WorkerExecutionToken, WorkerState};

pub(super) struct BottomHalfDrainSlot {
    pub(super) state: WorkerState,
    current: Option<WorkerExecutionToken>,
    generation: WorkerExecutionToken,
}

impl BottomHalfDrainSlot {
    pub(super) const fn new() -> Self {
        Self {
            state: WorkerState::Empty,
            current: None,
            generation: WorkerExecutionToken::from_usize(1),
        }
    }

    pub(super) fn install(&mut self) -> bool {
        if self.state != WorkerState::Empty {
            return false;
        }
        self.state = WorkerState::Idle;
        true
    }

    pub(super) fn reserve_execution_token(&mut self) -> WorkerExecutionToken {
        self.generation = self.generation.next_generation();
        self.generation
    }

    pub(super) const fn reserved_token_matches(&self, token: WorkerExecutionToken) -> bool {
        self.generation.as_usize() == token.as_usize()
    }

    pub(super) const fn current_matches(&self, token: WorkerExecutionToken) -> bool {
        matches!(self.current, Some(current) if current.as_usize() == token.as_usize())
    }

    pub(super) fn start_execution(&mut self, token: WorkerExecutionToken) {
        self.current = Some(token);
        self.state = WorkerState::Running;
    }

    pub(super) fn clear_execution(&mut self) {
        self.current = None;
    }
}
