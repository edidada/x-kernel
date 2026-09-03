# kworkerpool Design

## Role

`kworkerpool` owns the worker-pool scheduling machinery used below `kwork`.
It does not model workqueues, callbacks, or product-level pool kinds. Entries
are opaque source/owner/key/payload values. Product layers decide what those
values mean and validate them after the pool has selected a candidate.

The crate has two pool models:

- normal ktask-backed pools, represented by `KtaskWorkerPool` over the
  `WorkerPool` state machine;
- bottom-half pools, represented by `BottomHalfWorkerPool` and driven by an
  external bottom-half execution context.

The split is intentional. A normal pool owns worker tasks, dynamic worker
creation, idle retirement, a manager task, and scheduler accounting. A
bottom-half pool owns only queueing and one drain slot; the caller owns the
actual bottom-half raise and run context.

## Scope

The implementation is split by behavior instead of by product pool kind:

- `core/` contains shared entry identity, payload, worker identifiers,
  execution tokens, queue storage, errors, park decisions, and snapshots.
- `normal/` contains task-backed worker state, policy, action batches,
  ktask worker integration, manager integration, and execution-context
  accounting.
- `bottom_half/` contains the independent single-drain-slot state machine used
  when execution is triggered by an external bottom-half mechanism.

`kworkerpool` does not contain a `PoolKind` or `PoolId`. Runtime routing belongs
to the caller. Normal-pool task accounting uses an opaque internal
`WorkerPoolIdentity` so multiple normal pools on the same CPU cannot be confused
by scheduler tick, block, or resume callbacks.

## Architecture

```text
product layer, such as kwork
  |
  |  opaque PoolEntry values
  v
+-------------------- kworkerpool --------------------+
|                                                     |
|  normal/                                            |
|    KtaskWorkerPool<R>                               |
|      - SpinNoIrq<WorkerPool<WorkerThreadRef>>       |
|      - registered WorkerRuntimeFactory<R>           |
|      - worker wake sources                          |
|      - manager wake source                          |
|                                                     |
|    WorkerPool                                       |
|      - worker slot state machine                    |
|      - runnable FIFO and per-owner deferred queues  |
|      - concurrency and CPU-intensive accounting     |
|      - dynamic spawn and idle-retire decisions      |
|                                                     |
|  bottom_half/                                       |
|    BottomHalfWorkerPool                             |
|      - one drain slot                               |
|      - runnable FIFO and per-owner deferred queues  |
|      - no ktask worker and no manager               |
|                                                     |
+-----------------------------------------------------+
```

The normal ktask wrapper exists to keep side effects out of the pure state
machine. `WorkerPool` records decisions as data. `KtaskWorkerPool`, the worker
runtime, and the manager runtime turn those decisions into ktask creation,
wakeups, waits, and scheduler-accounting updates.

## Normal Worker State Machine

Normal worker slots move through the following states:

```text
Empty
  -> Creating
  -> Idle
  -> Preparing
  -> Claiming
  -> Running
  -> Sleeping
  -> Running
  -> Idle
  -> RetireRequested
  -> Exiting
  -> Empty
```

Important transitions:

- `Empty -> Creating` reserves a slot before the manager creates a ktask
  worker.
- `Creating -> Idle` completes a successful worker installation.
- `Idle -> Preparing` assigns an installed worker to runnable work.
- `Preparing -> Claiming` pops one FIFO runnable entry and reserves a
  `WorkerExecutionToken`.
- `Claiming -> Running` happens only after the product runtime accepts the
  opaque entry outside the pool lock.
- `Running -> Sleeping -> Running` tracks sleepable blocking by the current
  execution token.
- `Running -> Idle` happens when accepted work finishes.
- `Idle -> RetireRequested` is chosen by the manager when idle retirement is
  enabled and the pool is above `min_workers`.
- `RetireRequested -> Preparing` cancels pending retirement if work arrives
  before the worker exits.
- `RetireRequested -> Exiting -> Empty` releases a worker slot in the worker's
  own task context.

The token is part of every execution-sensitive transition. Tick, block, resume,
finish, commit, and discard operations must carry the token so stale callbacks
cannot mutate a reused slot.

## Normal Dispatch Flow

The normal dispatch fast path is deliberately small:

1. A producer enqueues or promotes a runnable entry while holding the pool lock.
2. The pool evaluates dispatch. If normal concurrency is available, it prefers
   an already preparing worker, then a retiring worker that can be rescued, then
   an idle worker.
3. If no installed worker can run and creation is allowed, the pool marks
   manager work pending and returns a manager wake action.
4. The caller drops the pool lock and applies the returned immediate actions.

The worker loop follows the same lock boundary:

1. It calls `worker_ready_to_park()` at the wait boundary.
2. If work may be available, it calls `prepare_runnable_candidate()` while
   holding the lock.
3. It drops the lock and asks the product runtime to validate the opaque entry.
4. A successful claim is committed and run. A stale claim is discarded and may
   retry another candidate.
5. After callback execution, the worker calls `worker_finished()` with the same
   execution token.

The pool owns FIFO ordering and worker-state transitions. The runtime owns
external object lookup, callback execution, wake implementation, and blocking
wait implementation.

## Actions And Manager Flow

Normal pool state transitions never perform runtime side effects while holding
the pool lock. They return an `ActionBatch` containing `ImmediateAction` values
that must be applied after unlocking.

`ImmediateAction` is a runtime side-effect request. It can be produced from
three contexts:

- producer paths, such as enqueue or promote, when new runnable work appears;
- worker paths, such as stale discard, finish, block, or idle wait decisions;
- manager paths, after a dynamic worker spawn succeeds or fails.

These actions do not carry pool identity. The caller must keep the source pool
handle together with the returned `ActionBatch` and apply the actions to that
same pool. This matches the state-machine boundary: actions are the result of a
specific pool transition, not a cross-pool routing message.

`ManagementAction` is different. It is crate-internal lifecycle work consumed by
the normal manager. The manager asks each pool for at most a small bounded
number of actions per pass:

- `SpawnWorker` means the pool already moved an empty slot to `Creating`; the
  manager must create the ktask worker and then call spawn completion.
- `RetireWorker` means the pool already moved an idle slot to
  `RetireRequested`; the manager wakes that worker so it can observe the exit
  decision in its own context.

The manager can serve more than one normal pool on the same placement domain.
Because `ManagementAction` is always fetched from one concrete pool and handled
against that same pool, it does not need a product-level pool id.

## Dynamic Worker Policy

Normal pools are scalable by construction. Scaling is bounded by
`min_workers`, `initial_workers`, and `max_workers`:

- `initial_workers` controls initial bring-up.
- `max_workers` caps dynamic creation.
- `min_workers` is the idle-retirement floor.

Worker creation is attempted only when runnable work exists, no normally
accounted worker is providing concurrency, no worker is preparing, no retirement
is pending, no creation is already in flight, and the installed plus creating
count is below `max_workers`.

After a creation failure, `create_retry_delay` delays the next creation attempt.
When `min_workers == max_workers`, the count check naturally disables dynamic
growth without needing a separate fixed-pool mode.

## CPU-Intensive Accounting

A running execution starts as normal concurrency. If scheduler ticks show that
the same execution exceeds `cpu_intensive_threshold`, it is marked
CPU-intensive. The execution continues running, but it no longer consumes the
pool's bounded normal-concurrency slot. If runnable work remains, the pool may
wake or create another worker.

The ktask integration stores these words in `ktask::TaskExecutionContext`:

```text
WorkerPoolIdentity, cpu_id, worker_id, execution_token
```

`WorkerPoolIdentity` is derived from the `KtaskWorkerPool` instance address and
is checked together with the placement CPU. This prevents scheduler accounting
for one normal pool from mutating another normal pool on the same CPU.

## Bottom-Half Flow

`BottomHalfWorkerPool` is not a ktask-backed normal pool with different policy.
It has its own single-slot state machine and never creates or wakes worker
threads.

The product layer initializes the drain slot once. When enqueue or promotion
makes work runnable, the product layer raises its own bottom-half mechanism.
The bottom-half context then:

1. calls `drain_ready_to_park()`;
2. prepares a runnable candidate;
3. validates the opaque entry outside the pool lock;
4. commits and runs accepted work, or discards stale work;
5. calls `drain_finished()` after accepted work completes.

This keeps shared queueing semantics available to bottom-half users while
excluding normal-only concepts such as worker ktasks, dynamic creation,
retirement, manager wakeups, and CPU-intensive scheduler accounting.

## Locking And Execution Context

`WorkerPool` and `BottomHalfWorkerPool` are state machines protected by their
wrappers' locks. Runtime callbacks, worker creation, wakeups, bottom-half
raising, and blocking waits must not run while the pool lock is held.

Normal worker and manager tasks require ktask scheduling to be available and
may sleep in task context. Bottom-half pool operations can be used by a
bottom-half integration layer, but callback execution must follow the caller's
bottom-half context rules.

The state machines are not reentrant for the same pool instance. Reentrancy
must be handled above the pool by preserving lock ordering and by applying
returned action batches only after leaving the locked section.
