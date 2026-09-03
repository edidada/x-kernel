// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel workqueue product API.
//!
//! `kwork` is the callback-facing layer used by ordinary kernel subsystems. It
//! owns [`ScheduledWork`] objects, delayed work, built-in system queues, dynamic
//! logical queues, and [`BudgetedPoller`]. Lower-level queue state lives in
//! `kworkqueue`, and worker scheduling lives in `kworkerpool`; users of this
//! crate normally should not call those crates directly.
//!
//! # Choosing An API
//!
//! - Use [`ScheduledWork::schedule`] or [`system_wq`] for ordinary sleepable
//!   task-context callbacks.
//! - Use [`system_percpu_wq`] or [`ScheduleAttrs::on_cpu`] when the callback must
//!   run on a selected CPU.
//! - Use [`WorkQueueHandle`] when a subsystem needs its own logical queue,
//!   ordered execution limit, or explicit teardown with [`WorkQueueHandle::destroy`].
//! - Use [`DelayedScheduledWork`] for timer-delayed callback execution.
//! - Use [`system_bh_wq`], [`system_bh_highpri_wq`],
//!   [`ScheduleAttrs::bottom_half`], or [`ScheduleAttrs::bottom_half_highpri`]
//!   only for callbacks that can run from bottom-half context and must not sleep.
//! - Use [`BudgetedPoller`] for NAPI-like coalesced polling where many producer
//!   notifications should collapse into bounded poll rounds.
//!
//! Dynamic queues are logical queues, not private worker pools. They currently
//! use the built-in normal task-context worker pools underneath, and
//! [`WorkQueueAttrs::with_max_active`] limits how many callbacks from that
//! logical queue may run concurrently.
//!
//! # Scheduling Normal Work
//!
//! [`ScheduledWork`] is reusable. One pending or running instance may exist at a
//! time; a second enqueue while it is already queued usually returns
//! [`QueueWorkResult::AlreadyQueued`]. After the callback finishes or is
//! cancelled, the same handle may be queued again.
//!
//! ```rust,ignore
//! use kwork::{QueueWorkResult, ScheduledWork};
//!
//! let work = ScheduledWork::new(|_work| {
//!     // Runs later in a sleepable worker task.
//! });
//!
//! match work.schedule() {
//!     QueueWorkResult::Queued | QueueWorkResult::AlreadyQueued => {}
//!     QueueWorkResult::Disabled => return,
//!     QueueWorkResult::QueueFull
//!     | QueueWorkResult::InvalidCpu
//!     | QueueWorkResult::WorkerUnavailable => return,
//! }
//!
//! work.flush().expect("flush requires sleepable task context");
//! ```
//!
//! # CPU-Targeted Work
//!
//! Use [`ScheduleAttrs::on_cpu`] for one-shot scheduling through a built-in
//! queue, or [`WorkQueue::queue_work_on`] / [`WorkQueueHandle::queue_work_on`]
//! when the caller already owns a queue.
//!
//! ```rust,ignore
//! use kcpu_id_map::LogicalCpuId;
//! use kwork::{ScheduleAttrs, ScheduledWork};
//!
//! let work = ScheduledWork::new(|_| {});
//! let _ = work.schedule_with(ScheduleAttrs::system().on_cpu(LogicalCpuId::new(0)));
//! ```
//!
//! # Dynamic Queues
//!
//! A dynamic queue gives a subsystem a private logical lane and teardown point.
//! `max_active = 1` provides serialized callback execution for that queue.
//!
//! ```rust,ignore
//! use kwork::{ScheduledWork, WorkQueueAttrs, WorkQueueHandle};
//!
//! let queue = WorkQueueHandle::alloc(
//!     "device_reset_wq",
//!     WorkQueueAttrs::new().with_max_active(1),
//! )
//! .expect("dynamic queue allocation");
//!
//! let reset = ScheduledWork::new(|_| {});
//! let _ = queue.queue_work(&reset);
//! queue.flush().expect("flush requires task context");
//! queue.destroy().expect("destroy waits for queued work");
//! ```
//!
//! # Delayed Work
//!
//! Delayed work wraps a [`ScheduledWork`] with timer state. A zero delay is
//! converted to immediate enqueue. [`DelayedScheduledWork::mod_schedule_after_with`]
//! cancels only pending delayed state before scheduling the new delay; it does
//! not wait for a running callback.
//!
//! ```rust,ignore
//! use ktime_types::TimeSpan;
//! use kwork::{DelayedScheduledWork, ScheduleAttrs};
//!
//! let retry = DelayedScheduledWork::new(|_| {});
//! let _ = retry.schedule_after_with(TimeSpan::from_millis(10), ScheduleAttrs::system());
//! let _ = retry.mod_schedule_after_with(TimeSpan::from_millis(50), ScheduleAttrs::system());
//! retry.cancel_sync().expect("cancel_sync requires task context");
//! ```
//!
//! # Bottom-Half Work
//!
//! Bottom-half queues are drained from softirq context. Their callbacks must not
//! sleep, block on workqueue completion, allocate in ways that are invalid from
//! the current context, or call APIs that require task context.
//!
//! ```rust,ignore
//! use kwork::{QueueWorkResult, ScheduledWork};
//!
//! let work = ScheduledWork::new(|_| {
//!     // Must remain non-sleeping bottom-half work.
//! });
//!
//! if work.schedule_with(kwork::ScheduleAttrs::bottom_half()) == QueueWorkResult::Queued {
//!     // The runtime raises the built-in bottom-half drain context.
//! }
//! ```
//!
//! # Cancellation And Blocking
//!
//! [`ScheduledWork::cancel`] and [`DelayedScheduledWork::cancel`] are
//! non-blocking. They remove pending work when possible and return
//! [`CancelWorkResult::Running`] if the callback is already executing.
//!
//! [`ScheduledWork::flush`], [`ScheduledWork::cancel_sync`],
//! [`DelayedScheduledWork::cancel_sync`], [`WorkQueue::flush`],
//! [`WorkQueueHandle::flush`], and [`WorkQueueHandle::destroy`] may block and
//! must be called from sleepable task context. They return
//! [`WorkqueueError::InvalidContext`] from interrupt-like contexts and may return
//! [`WorkqueueError::WouldDeadlock`] when a normal worker would wait on work
//! backed by its own bounded per-CPU worker pool.
//!
//! [`ScheduledWork::disable`] prevents future enqueue attempts until matching
//! [`ScheduledWork::enable`] calls re-enable the work. Disable depth is counted;
//! overflow returns [`DisableWorkError::Overflow`].
//!
//! # Budgeted Polling
//!
//! [`BudgetedPoller`] coalesces repeated producer notifications into at most one
//! active poll executor. Call [`BudgetedPoller::start`] from task context before
//! producers publish work. Producers can then call
//! [`BudgetedPoller::notify_irq_safe`] from IRQ-adjacent paths.
//!
//! ```rust,ignore
//! use kwork::{BudgetedPollProgress, BudgetedPoller};
//!
//! let poller = BudgetedPoller::new("net_rx_poll", 64usize, 16usize, 8, |budget| {
//!     let has_more = poll_rx_packets(budget);
//!     BudgetedPollProgress { has_more }
//! });
//!
//! poller.start().expect("poller start");
//! let queued_or_running = poller.notify_irq_safe();
//! if !queued_or_running {
//!     poller.assist_once();
//! }
//! poller.destroy().expect("destroy waits for poll callbacks");
//! # fn poll_rx_packets(_budget: usize) -> bool { false }
//! ```
//!
//! # Runtime Glue
//!
//! `kwork::raw` is hidden startup and watchdog glue used by runtime crates. It is
//! not the normal API for subsystem work submission.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod budgeted_poller;
mod builtinpool;
mod builtinwq;
mod healthcheck;
mod runtime;
#[cfg(feature = "stress_test")]
mod stress;
mod work;

pub use budgeted_poller::{BudgetedPollProgress, BudgetedPoller, BudgetedPollerStartError};
pub use builtinwq::{system_bh_highpri_wq, system_bh_wq, system_percpu_wq, system_wq};
#[cfg(feature = "stress_test")]
pub use stress::{StressError, StressSummary, run_stress_command, stress_status_text};
pub use work::{
    CancelWorkResult, DEFAULT_WORKQUEUE_MAX_ACTIVE, DelayedScheduledWork, DisableWorkError,
    EnableWorkError, MAX_WORKQUEUE_PENDING, QueueDelayedWorkResult, QueueWorkResult, ScheduleAttrs,
    ScheduledWork, WorkQueue, WorkQueueAllocError, WorkQueueAttrs, WorkQueueFlags, WorkQueueHandle,
    WorkQueueMaxActive, WorkqueueError,
};

/// Low-level workerqueue runtime surface used by startup and watchdog glue.
#[doc(hidden)]
pub mod raw {
    pub use crate::{
        builtinpool::{BuiltinPoolInitError, SystemWorkerPoolInitResult},
        healthcheck::system_workqueue_watchdog_check,
        runtime::{init_bottom_half_workerqueue, init_system_workqueue_worker_pools_for_cpu},
    };
}
