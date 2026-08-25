// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Threaded IRQ handler state.

use alloc::{boxed::Box, string::String, sync::Arc};
#[cfg(unittest)]
use core::sync::atomic::{AtomicBool, Ordering};

use kpoll::Completion;
use kspin::SpinNoIrq;

use super::sync_wait::IrqSyncWaitIf;
use crate::{Virq, platform::Handler};

#[cfg(unittest)]
static FAIL_NEXT_THREAD_SPAWN: AtomicBool = AtomicBool::new(false);

#[cfg(unittest)]
pub(super) fn fail_next_spawn_for_tests() {
    FAIL_NEXT_THREAD_SPAWN.store(true, Ordering::Release);
}

/// Scheduler bridge used to spawn IRQ handler threads.
///
/// `kirq` owns the threaded IRQ lifecycle, while the task subsystem owns task
/// creation and scheduling. This provider keeps that dependency one-way:
/// schedulers provide an IRQ thread executor without making the IRQ core depend
/// on `ktask`.
#[kiface::interface]
pub trait IrqThreadTaskIf {
    /// Spawns one scheduler-visible IRQ thread.
    ///
    /// The entry is expected to run until the IRQ action is freed.
    fn spawn_irq_thread(name: String, entry: Box<dyn FnOnce() + Send>) -> bool;
}

/// Per-action threaded IRQ slot.
#[derive(Clone)]
pub(super) struct IrqThreadSlot {
    inner: Arc<IrqThreadState>,
}

struct IrqThreadState {
    virq: Virq,
    handler: Handler,
    state: SpinNoIrq<IrqThreadData>,
    ready: Completion,
    wake: Completion,
    idle: Completion,
    exited: Completion,
}

struct IrqThreadData {
    pending: bool,
    running: bool,
    stopping: bool,
    sources: u8,
}

impl IrqThreadData {
    const fn new() -> Self {
        Self {
            pending: false,
            running: false,
            stopping: false,
            sources: 0,
        }
    }
}

enum IrqThreadNext {
    Run { sources: u8 },
    Wait,
    Stop,
}

impl IrqThreadSlot {
    pub(super) fn spawn(virq: Virq, handler: Handler, name: Option<&'static str>) -> Option<Self> {
        #[cfg(unittest)]
        if FAIL_NEXT_THREAD_SPAWN.swap(false, Ordering::AcqRel) {
            return None;
        }

        let slot = Self {
            inner: Arc::new(IrqThreadState::new(virq, handler)),
        };
        let thread_slot = slot.clone();
        let thread_name = name
            .map(String::from)
            .unwrap_or_else(|| alloc::format!("irq/{virq}"));
        if !IrqThreadTaskIf::spawn_irq_thread(
            thread_name,
            Box::new(move || {
                thread_slot.run_loop();
            }),
        ) {
            return None;
        }
        if slot.wait_ready().is_err() {
            slot.stop();
            let _ = slot.wait_stopped("spawn_irq_thread_ready");
            return None;
        }
        Some(slot)
    }

    pub(super) fn wake(&self, sources: u8) {
        self.inner.wake(sources);
    }

    pub(super) fn wait_idle(
        &self,
        operation: &'static str,
    ) -> Result<(), kpoll::PollRegisterError> {
        self.inner.wait_idle(operation)
    }

    pub(super) fn wait_stopped(
        &self,
        operation: &'static str,
    ) -> Result<(), kpoll::PollRegisterError> {
        self.inner.wait_stopped(operation)
    }

    pub(super) fn stop(&self) {
        self.inner.stop();
    }

    fn wait_ready(&self) -> Result<(), kpoll::PollRegisterError> {
        self.inner.wait_ready()
    }

    fn run_loop(&self) {
        self.inner.run_loop();
    }

    #[cfg(unittest)]
    pub(super) fn test_new(virq: Virq, handler: Handler) -> Self {
        Self {
            inner: Arc::new(IrqThreadState::new(virq, handler)),
        }
    }
}

impl IrqThreadState {
    fn new(virq: Virq, handler: Handler) -> Self {
        let idle = Completion::new();
        idle.complete_all();
        Self {
            virq,
            handler,
            state: SpinNoIrq::new(IrqThreadData::new()),
            ready: Completion::new(),
            wake: Completion::new(),
            idle,
            exited: Completion::new(),
        }
    }

    fn wake(&self, sources: u8) {
        {
            let mut state = self.state.lock();
            if state.stopping {
                return;
            }
            if !state.pending && !state.running {
                self.idle.reinit();
            }
            state.pending = true;
            state.sources |= sources;
        }
        self.wake.complete_all();
    }

    fn stop(&self) {
        {
            let mut state = self.state.lock();
            state.stopping = true;
            if state.pending || state.running {
                self.idle.reinit();
            }
        }
        self.wake.complete_all();
    }

    fn wait_idle(&self, operation: &'static str) -> Result<(), kpoll::PollRegisterError> {
        loop {
            let idle = {
                let state = self.state.lock();
                !state.pending && !state.running
            };
            if idle {
                return Ok(());
            }
            IrqSyncWaitIf::wait_for_completion(&self.idle).inspect_err(|error| {
                warn!("threaded IRQ wait for {operation} failed: {error:?}");
            })?;
        }
    }

    fn wait_stopped(&self, operation: &'static str) -> Result<(), kpoll::PollRegisterError> {
        if self.exited.is_completed() {
            return Ok(());
        }
        IrqSyncWaitIf::wait_for_completion(&self.exited).inspect_err(|error| {
            warn!("threaded IRQ wait for {operation} stop failed: {error:?}");
        })
    }

    fn wait_ready(&self) -> Result<(), kpoll::PollRegisterError> {
        IrqSyncWaitIf::wait_for_completion(&self.ready).inspect_err(|error| {
            warn!(
                "threaded IRQ {} readiness wait failed: {error:?}",
                self.virq
            );
        })
    }

    fn run_loop(&self) {
        self.ready.complete_all();
        loop {
            match self.next_work() {
                IrqThreadNext::Run { sources } => {
                    let _ = sources;
                    self.handler.handle(self.virq);
                    self.finish_run();
                }
                IrqThreadNext::Wait => {
                    if IrqSyncWaitIf::wait_for_completion(&self.wake).is_err() {
                        warn!("threaded IRQ {} wait provider failed", self.virq);
                        self.stop();
                    }
                }
                IrqThreadNext::Stop => break,
            }
        }
        self.exited.complete_all();
    }

    fn next_work(&self) -> IrqThreadNext {
        let mut state = self.state.lock();
        if state.stopping && !state.pending {
            return IrqThreadNext::Stop;
        }
        if state.pending {
            let sources = state.sources;
            state.pending = false;
            state.running = true;
            state.sources = 0;
            self.wake.reinit();
            return IrqThreadNext::Run { sources };
        }
        self.wake.reinit();
        IrqThreadNext::Wait
    }

    fn finish_run(&self) {
        let complete_idle = {
            let mut state = self.state.lock();
            state.running = false;
            !state.pending
        };
        if complete_idle {
            super::manager::finalize_oneshot_thread(self.virq);
            self.idle.complete_all();
        }
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::IrqEvent;

    static THREAD_SLOT_TEST_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn test_slot_handler(_irq: Virq) -> IrqEvent {
        THREAD_SLOT_TEST_CALLS.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    }

    #[def_test]
    fn test_thread_slot_keeps_wake_queued_before_run_loop_is_ready() {
        THREAD_SLOT_TEST_CALLS.store(0, Ordering::Relaxed);
        let slot = IrqThreadSlot::test_new(0x5a5a, Arc::new(test_slot_handler));

        slot.wake(0b1010);
        slot.stop();
        slot.run_loop();

        assert_eq!(THREAD_SLOT_TEST_CALLS.load(Ordering::Relaxed), 1);
    }
}
