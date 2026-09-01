# kwork Design

## Role

`kwork` is the kernel-facing workqueue product layer. It exposes callback work
objects, delayed work, dynamic logical queues, built-in system queues,
bottom-half queues, budgeted polling, stress hooks, and watchdog integration.

The crate assembles two lower-level cores:

- `kworkqueue` owns logical queue and work-item state.
- `kworkerpool` owns worker scheduling, worker lifecycle, dispatch, and
  ktask-backed worker/manager loops.

`kwork` owns the concrete callback storage and the built-in mapping between
logical queues and execution pools. It depends directly on `ktask` for task
workers, waits, and timers, and on `kirq` for bottom-half execution and
interrupt-context checks.

## Public API

The crate root intentionally exposes only product-level APIs:

- `ScheduledWork` is a reusable callback work item.
- `DelayedScheduledWork` is delayed work with generation-protected timer state.
- `WorkQueue` is a static logical queue.
- `WorkQueueHandle` is an allocated dynamic logical queue owner.
- `ScheduleAttrs` selects the target queue family and optional CPU.
- `system_wq()`, `system_percpu_wq()`, `system_bh_wq()`, and
  `system_bh_highpri_wq()` return built-in queue objects.
- `BudgetedPoller` provides a NAPI-like coalesced polling helper.
- `PollLease` grants one task-context holder temporary responsibility for
  advancing a configured `BudgetedPoller`.
- `kwork::raw` is hidden glue for boot-time initialization and watchdog checks.

Built-in pool handles, pool kinds, runtime workers, bottom-half drain bindings,
and scheduler accounting helpers are internal implementation details.

## Basic Usage

Task-context work:

```rust
use kwork::{ScheduledWork, system_wq};

let work = ScheduledWork::new(|_work| {
    // Runs later in a sleepable worker task.
});

let _ = system_wq().queue_work(&work);
let _ = work.flush();
```

CPU-targeted work:

```rust
use kcpu_id_map::LogicalCpuId;
use kwork::{ScheduleAttrs, ScheduledWork};

let work = ScheduledWork::new(|_| {});
let _ = work.schedule_with(ScheduleAttrs::system().on_cpu(LogicalCpuId::new(0)));
```

Delayed work:

```rust
use ktime_types::TimeSpan;
use kwork::{DelayedScheduledWork, ScheduleAttrs};

let work = DelayedScheduledWork::new(|_| {});
let _ = work.schedule_after_with(TimeSpan::from_millis(10), ScheduleAttrs::system());
let _ = work.cancel_sync();
```

Bottom-half work:

```rust
use kwork::{ScheduleAttrs, ScheduledWork};

let work = ScheduledWork::new(|_| {
    // Runs from the bottom-half drain context. It must not sleep.
});

let _ = work.schedule_with(ScheduleAttrs::bottom_half());
```

## Built-In Topology

Each CPU owns one normal built-in worker pool and one bottom-half worker pool.

```text
system_wq/default selector ----+
system_percpu_wq/current CPU --+--> normal pool on CPU N
dynamic/static WorkQueue ------+

system_bh_wq ------------------+
system_bh_highpri_wq ----------+--> bottom-half pool on CPU N
```

`system_wq()` uses a ready-pool rotation for default placement so
interrupt-heavy producers do not permanently overload the current CPU. Explicit
CPU targeting uses the selected CPU. `system_percpu_wq()` and dynamic queues use
the current CPU unless `ScheduleAttrs::on_cpu()` or `queue_work_on()` is used.

Normal pool callbacks run in ktask-backed workers. Bottom-half callbacks run
from registered softirq drain actions. Both pool kinds use the same
`kworkerpool` entry and accounting model; only the execution domain differs.

## State Ownership

`ScheduledWork` stores the callback and owns the public lifecycle handle.
`kworkqueue::Work` stores queue-visible work state. Queue entries carry opaque
owner/key/payload values into `kworkerpool`. When a worker claims an entry,
`kwork` maps the entry back to the queue binding and scheduled work handle,
then runs the callback with internal locks released.

`ScheduledWork` and `DelayedScheduledWork` are reference-counted internally.
Pending entries, running callbacks, and delayed timers hold their own handle
clone, so the work object remains alive until the asynchronous instance is no
longer observable.

## Context Rules

Immediate enqueue paths may be called from task context, hardirq, softirq, or
BH-disabled context after the work object has already been created. They do not
run callbacks, create worker tasks, or allocate work objects.

`cancel()` is non-blocking. It may remove pending queued or delayed work from
IRQ-adjacent contexts and returns `Running` instead of waiting when a callback
is already executing.

Sleepable APIs such as `flush()`, `cancel_sync()`, and dynamic queue
`destroy()` require sleepable task context. `kwork` checks `kirq::context`
before blocking and returns `WorkqueueError::InvalidContext` from
interrupt-like contexts. A normal worker callback that would wait on work owned
by its own per-CPU normal pool receives `WorkqueueError::WouldDeadlock`.

Non-zero delayed work scheduling creates a timer handle and therefore requires
a context where allocation and ktask timer setup are valid. A zero delay is
converted to immediate enqueue.

`BudgetedPoller::notify_irq_safe()` remains valid in IRQ-adjacent contexts.
Lease acquisition and `PollLease::poll_once()` require task context because the
poll callback may take sleepable subsystem locks. A caller must stop and drop
all leases and foreground assists before destroying the poller.

## Progress Model

Enqueue commits queue state first, then submits an executor operation to the
selected built-in pool. If the pool is not ready or full, the work state is
rolled back and a typed result is returned.

Running callbacks do not hold workqueue locks. Completion updates the
`kworkqueue` binding state, promotes inactive work when active capacity opens,
and applies the resulting executor operations to the built-in pool.

Scheduler accounting uses `ktask::TaskExecutionContext` as an opaque
pool/worker/token identity. `kworkerpool` marks long-running task-context
workers CPU-intensive after the policy threshold, removes them from bounded
concurrency accounting, and asks the manager to create another worker when
queued work needs progress.

## Budgeted Poller Leases

`BudgetedPoller` stores executor state, a six-bit lease count, and the publish
generation in one `AtomicUsize`:

```text
bits 0..2    Idle / Scheduled / Running / RunningPending
bits 2..8    lease count
bits 8..     publish generation
```

Version one admits one lease. Keeping six bits in the representation reserves
space for later per-CPU or per-device ownership without changing the state-word
layout.

`new_with_lease()` adds a foreground budget and freshness timeout while
`new()` preserves the background-plus-assist configuration. A successful
`try_lease()` sets the lease count and establishes the first freshness
deadline. Queue installation publishes a separate atomic ready flag, so
`try_lease()` rejects the allocation window and destruction without taking the
queue spin lock. `PollLease::poll_once()` competes for the same `Scheduled` to
`Running` transition as background and assist executors. After a successful
round it refreshes the deadline before completion decides whether to queue
another background batch.

Notifications always publish state. A fresh lease changes only the queueing
decision: `Scheduled` remains visible for the holder, and neither notification
nor run completion queues another worker. The state-word update returned by
notification and lease release gives each queueing decision one linearization
point. A concurrent redundant queue attempt remains safe because only one
executor can acquire `Running`.

Dropping the final lease queues accumulated `Scheduled` work. An expired lease
also stops suppressing later notifications, and `kick_if_stale()` lets a
periodic caller recover scheduled work when no new producer arrives. The
freshness deadline is a separate `AtomicU64`; a race around deadline refresh
can only cause a redundant background queue attempt.

The ownership cleanup guarantee covers ordinary Rust control flow, including
early returns and propagated errors. A fatal abort does not execute `Drop`;
system reset owns recovery from that terminal state.

## Stress and Diagnostics

The optional stress subsystem is behind `KFEAT_KWORK_STRESS` and is triggered
through `/proc/kwork_stress`. It exercises public `kwork` APIs only. Boot never
runs stress automatically.

The watchdog hook scans built-in pool snapshots and reports a stuck pool only
after runnable work remains without execution progress for the configured
threshold.
