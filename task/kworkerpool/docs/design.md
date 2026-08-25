# kworkerpool Design

`kworkerpool` owns worker-pool execution state. It does not own workqueue
policy, pool bindings, flush colors, barriers, or task creation details.

## Scope

The first stage supports built-in static per-CPU pools:

- `Normal`: task-context workers, sleepable callbacks, dynamic worker
  expansion and idle retirement.
- `Bh`: bottom-half execution, non-sleepable drain context, no provider-created
  worker lifecycle.

`PoolKind` is a context and policy template. It is not a workqueue name. The
first stage does not support external dynamic pool registration.

## Pool Instance

Each pool instance owns:

- worker lifecycle state and counters,
- one runnable FIFO queue,
- deferred queues partitioned by caller-provided owner keys,
- pool policy and management request state.

`kworkerpool` does not interpret owner keys. A user such as `kwork` may use an
owner key to represent a workqueue-pool binding, but the worker-pool crate only
uses it for equality and mechanical promotion:

```text
enqueue_deferred(owner, entry)
promote_deferred(owner, budget)
```

The promotion policy remains outside `kworkerpool`.

## Queue Lanes

Runnable entries are eligible for immediate worker execution and are stored in a
single FIFO per pool instance.

Deferred entries are submitted to the pool but are not eligible for execution.
They are grouped by owner. The pool provides bounded storage and mechanical
promotion from one owner's deferred FIFO into the runnable FIFO. It does not
know why an entry is deferred or when it should become runnable.

## Actions

Fast-path actions are lightweight notifications for existing execution
contexts:

- wake an existing worker,
- raise bottom-half drain context,
- arm worker-related timers,
- wake the per-CPU manager.

Management actions are lifecycle slow paths:

- spawn a worker,
- request idle worker retirement.

The manager executes management actions but does not own policy. Pool instances
decide actions from state and policy.

## Manager

The first-stage manager model is one manager per CPU. It manages only pools with
provider-created worker lifecycle. `Bh` pools are excluded because they do not
spawn or retire worker tasks; their wake path raises bottom-half drain directly.

Managers use a pending-pool bitmap and process pending pools in batches. The
initial per-pool pass budget is two management actions.

## Worker State

Worker lifecycle states are:

- `Empty`: no provider task is associated with this slot.
- `Creating`: manager reserved the slot and is creating the task.
- `Idle`: task exists and may be selected for work.
- `RetireRequested`: manager asked an idle task to retire; this may be
  cancelled before the task begins exiting.
- `Exiting`: task has begun provider-side exit and cannot be recovered.
- `Preparing`: worker was selected and wake is in flight.
- `Running`: worker owns a current execution.
- `Sleeping`: worker owns a current execution but is blocked in a sleepable
  wait.

CPU-intensive is not a worker lifecycle state. It is current-execution
concurrency accounting: a long-running callback continues executing but no
longer counts as a worker that can advance the pool runnable queue.

## Locking Contract

The crate is structured as locked and unlocked layers. Public runtime-facing
methods acquire the pool lock in the eventual integration. Internal methods
with `_locked` suffix require the lock to be held and never call provider code.

State transitions must go through worker-state helpers so counters remain
consistent. Wake, spawn, retire, timer, and waiter notifications are executed
after locks are dropped.

Cross-object workqueue transactions remain outside this crate. `kwork` owns
pool-binding state, max-active policy, flush, barriers, and work-state claim
logic.

## kwork Migration Notes

The current `kwork` implementation stores `PendingWorkStore`, worker lifecycle
state, CPU-intensive accounting, and running-barrier handling inside
`WorkerPoolState`. Migration should be staged so workqueue semantics do not
leak into `kworkerpool`:

1. Introduce `kworkerpool` as an independent tested crate.
2. Adapt `kwork` entries into `PoolEntry<Owner, Key, Payload>` where `Owner`
   represents the workqueue-pool binding key and `Payload` is an opaque
   runnable handle meaningful only to `kwork`.
3. Keep `kwork::WorkQueuePoolBinding` as the cross-object coordinator. It owns
   max-active, inactive promotion policy, flush colors, barriers, and work-state
   claim/cleanup.
4. Replace `PendingWorkStore` storage with `kworkerpool` runnable/deferred
   lanes, but let `kwork` decide whether to enqueue runnable or deferred and
   when to call `promote_deferred(owner, budget)`.
5. Split pop and claim: `kworkerpool` pops a runnable entry; `kwork` claims the
   work state. If claim fails, `kwork` handles the stale entry and calls the
   appropriate worker-pool continuation API.
6. Move worker lifecycle state and resize decisions to `kworkerpool`. Provider
   actions remain executed by `ktask`.

The first integration cut should avoid changing flush/barrier behavior and
should preserve existing `kwork` tests before enabling idle retirement in
production paths.
