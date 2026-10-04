# kasyncio design

## Purpose

`kasyncio` owns the asynchronous submission/completion-ring I/O model
(issue #IKJIGT, phase 1): the per-process async context, the operation
staging queues, and the in-process worker threads that execute submitted
operations off the submitting user thread. Syscall ABI decoding for the
`asyncio_setup`/`asyncio_enter` entry points stays in `ksyscall`; the rings
and their user mapping land with those steps.

The synchronous syscall surface is untouched; async entry is additive and
incrementally enabled (read, write, fsync, splice, sendfile,
copy_file_range first).

Non-goals: io_uring ABI compatibility, kernel-side polling of the
submission ring (SQPOLL), cancellation requests from userspace, buffer
registration, and a preemptive async runtime.

## Scope

```text
process/kasyncio/src/
├── abi.rs       user ABI: opcodes, flags, SQE/CQE/params layouts, ring header
├── context.rs   AsyncContext: ready/completed staging, worker accounting, wake sources
├── worker.rs    spawn, idle park, retirement, signal/exit contracts
└── lib.rs       re-exports
```

Planned with the syscall step (not part of the tree yet): `file.rs`
(anon-inode fops: poll/register_poll/release), `ops.rs` (operation
decode/dispatch), `ring.rs` (SharedPages ring memory and kernel-side
accessors), and `setup.rs` (context creation and user mapping).

## Execution model

Workers are **sibling threads of the submitting process**, spawned through
the ordinary clone path (`Thread::prepare_thread_clone` →
`TaskInner::new_user` → `publish_user_task().commit()`) with the process
page-table root installed and a pure kernel-mode entry loop. This is what
makes user-buffer operations sound without mm-adoption machinery: a worker
dereferences user VAs through the same guarded user-memory path as a
syscall, page faults resolve against the worker's own process, and the fd
table and credentials are shared by construction.

Growth is lazy: the submit path reserves a slot and spawns only when queued
work exceeds idle workers, bounded by `WORKER_CAP` (4) per context. The
eligibility check and the slot charge share one critical section
(`try_reserve_worker`), so concurrent submitters cannot oversubscribe the
cap; a failed spawn releases the reservation.

### Visibility and accounting (intended semantics)

Unlike io_uring's io-wq kernel threads, a kasyncio worker is a **real
published thread of the process**:

- its tid is globally published and `tgkill`/`kill` can target it;
- it appears in process task listings and PID directories;
- its CPU time is charged to the process;
- it participates in process thread membership (see the exit contract).

Phase 1 declares these as intended behavior, not incidental artifacts.
Hiding workers from signals/listings/accounting (a closer io_uring
analogue) is a design decision reserved for the syscall-integration step;
doing it later would change observable ABI surface, so it is called out
here explicitly to avoid rework under an ABI-compatibility pretext.

## Locking and wake contract

- `AsyncContext::inner` is a sleepable `ksync::Mutex` guarding short,
  non-blocking bookkeeping; nothing waits while holding it. Workers and
  submitters both run in sleepable task context and no IRQ handler touches
  this state, so no spinlock class is required.
- Two `PollSet` wake sources follow the detach-then-wake rule and are never
  woken under a lock: `worker_wake` (queued work or shutdown) and
  `cq_wake` (published completions).
- The ready queue is bounded by `READY_LIMIT` (1024); a full queue reports
  `WouldBlock` to the submitter.

## Completion contract

Every operation **accepted onto the ready queue** has exactly one of three
outcomes, and its user cookie is observable in exactly one completion:

- **Executed**: a worker dispatched it; the completion carries the natural
  result (non-negative count or negated errno).
- **Cancelled**: `shutdown` found it still queued (never dispatched); the
  completion carries `ASYNCIO_RES_CANCELED` (negated `ECANCELED`).
- **In flight at shutdown**: already dispatched operations run to their
  natural completion even after shutdown; the context stays alive until
  the last worker retires.

A submission rejected at enqueue produces no completion — the submitter
observes the error directly (`WouldBlock` on a full queue,
`BadFileDescriptor` on a shut-down context).

## Worker exit contract

Workers retire — run the loop exit, then `posix_process::do_exit(0, false)`
so membership, cgroup charge, and (when last) the process tail are handled
by the ordinary exit path — when any of the following holds:

1. **Context shutdown** (fd `release` or test): never blocks; the shutdown
   also cancels undispatched queue entries per the completion contract and
   wakes both `PollSet`s.
2. **Thread exit / process group exit**: the group-exit SIGKILL broadcast
   reaches workers through the same `interruptible` waits the synchronous
   syscalls use, so blocked operations unwind with `-EINTR`.
3. **Pending `SIGKILL`**: a worker never re-enters userspace, where
   fatal-signal delivery normally happens, so the idle path probes the
   thread's pending-signal set after every signal-induced wake. `kill -9`
   on a worker tid therefore ends the worker. Handler-visible fatal
   signals other than `SIGKILL` still require a userspace return and are
   out of scope for phase 1.
4. **Last ordinary thread gone**: the final ordinary (non-worker) thread's
   plain exit must not strand parked workers holding membership — that
   would stall `waitpid` forever. Workers re-evaluate at least once per
   `IDLE_PARK_BOUND` (200 ms) whether `Process::has_only_in_process_workers`
   holds — an explicit per-process worker counter maintained by the spawn
   (attach after membership publication) and retire (detach before
   membership removal) paths, so it cannot be forged through task naming
   and is conservative under races. The last `do_exit` then completes the
   process tail. This also removes the need for a cross-context notifier
   when a process owns several async contexts.

## Known limitations

- Multiple contexts per process each park independently; the periodic
  park bound (not a cross-context wake) is what eventually retires their
  workers. Latency in that corner is bounded but not prompt.
- The completion path stages completions in a kernel-side deque until the
  completion ring lands; ring publication replaces the staging step.
- Handler-visible fatal signals other than `SIGKILL` still require a
  userspace return; workers only self-terminate on pending `SIGKILL`.
