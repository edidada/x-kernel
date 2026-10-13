// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel waiting provider for portable block transactions.

use alloc::{boxed::Box, sync::Arc};

use kerrno::{KError, KResult};
use kpoll::{Completion, PollRegisterError, PollRegistration};
use ktask::future::PreparedTaskWait;

use crate::completion::{BlockSignals, BlockWaiter};

pub fn prepare_block_wait() -> KResult<Box<dyn BlockWaiter>> {
    Ok(Box::new(HostBlockWaiter::prepare()?))
}

struct HostSignals {
    admission: Completion,
    terminal: Completion,
}

impl BlockSignals for HostSignals {
    fn notify_admission(&self) {
        self.admission.complete();
    }

    fn notify_completion(&self) {
        self.terminal.complete_all();
    }
}

struct HostBlockWaiter {
    // Cancel the registration before releasing either the source or the task.
    _terminal_registration: PollRegistration,
    signals: Arc<HostSignals>,
    task: PreparedTaskWait,
}

impl HostBlockWaiter {
    fn prepare() -> KResult<Self> {
        let task = PreparedTaskWait::prepare().map_err(|_| KError::InvalidInput)?;
        let signals = Arc::new(HostSignals {
            admission: Completion::new(),
            terminal: Completion::new(),
        });
        let waker = task.waker();
        let registration = signals
            .terminal
            .register_owned(&waker)
            .map_err(registration_error)?;
        Ok(Self {
            _terminal_registration: registration,
            signals,
            task,
        })
    }
}

impl BlockWaiter for HostBlockWaiter {
    fn signals(&self) -> Arc<dyn BlockSignals> {
        self.signals.clone()
    }

    fn wait_admission(&mut self) -> KResult<()> {
        let waker = self.task.waker();
        loop {
            if self.signals.admission.try_wait() {
                return Ok(());
            }
            let _registration = self
                .signals
                .admission
                .register_owned(&waker)
                .map_err(registration_error)?;
            if self.signals.admission.try_wait() {
                return Ok(());
            }
            self.task.park();
        }
    }

    fn wait_completion(&mut self) {
        // The private terminal source never resets. Keep its initial
        // registration even when admission or an unrelated task wake fires.
        while !self.signals.terminal.try_wait() {
            self.task.park();
        }
    }
}

fn registration_error(error: PollRegisterError) -> KError {
    match error {
        PollRegisterError::NoMemory | PollRegisterError::IdExhausted => KError::NoMemory,
        PollRegisterError::InvalidState => KError::InvalidInput,
    }
}
