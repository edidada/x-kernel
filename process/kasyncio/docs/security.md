# kasyncio security

## Trust boundary

Everything that crosses the user/kernel boundary here is untrusted until
validated:

- **SQE contents** (once the submission ring lands): opcode, flags, fds,
  lengths, offsets, and user pointers arrive from userspace. The submit
  path resolves fds through the caller's fd table with normal permission
  checks and validates flag bits and reserved fields before an operation
  is queued; unknown bits are rejected, not ignored.
- **User buffers**: operations carrying user VAs execute on a worker that
  is a thread of the owning process, so every access goes through the
  standard guarded user-memory path (`osvm`/`kuaccess`) with fault
  resolution against the worker's own address space. No worker ever
  dereferences a raw user pointer outside that path, and no other
  process's memory is reachable.
- **Ring memory**: ring pages are user-mappable and writable by design. The
  kernel keeps its own `sq_head`/`cq_tail`, masks every user-published index,
  validates every SQE (supported opcode, zero `flags`, bounded array index),
  and only publishes CQEs it generated. The pages are owned by the open file,
  so the descriptor must stay open while its mappings are in use (liburing's
  `io_uring_queue_exit` munmaps before closing); closing the last descriptor
  stops workers and drains staged completions.
- **`io_uring_enter` arguments**: unknown enter flags and a non-NULL signal
  mask are rejected; `to_submit` beyond the user-published tail or the ring
  depth is rejected. A subset that needs interruptible enter must add signal
  masking rather than silently ignoring it.

## Worker credentials and reach

A worker is a sibling thread of the submitting process, so it inherits —
by ordinary thread semantics, not by any kasyncio grant — the process
address space, fd table, and credentials. This is the same trust
relationship as any pthread the process could create: kasyncio does not
elevate, and cannot be used to escalate, what the process can already do
to itself. Workers never enter userspace, so they never execute attacker
-supplied code; they only execute the kernel's own operation dispatch.

Worker tids are globally published (see `design.md`, visibility and
accounting). `kill`/`tgkill` targeting a worker tid therefore requires the
signal permission checks that already govern inter-process signaling —
unchanged from any other thread.

## Resource bounds

- `WORKER_CAP` (4) bounds kernel stacks per context; the reservation is
  charged in the same critical section as the eligibility check, so
  concurrent submitters cannot oversubscribe it.
- `READY_LIMIT` (1024) bounds the per-context queue; a full queue reports
  `WouldBlock` to the submitter instead of growing.
- Idle-park registration failures back off exponentially (10 ms doubled to
  a 320 ms ceiling) so a persistently failing wake source cannot spin the
  CPU.
- Workers retire on shutdown, process/group exit, pending `SIGKILL`, or
  the last ordinary thread leaving, so contexts cannot strand threads or
  pin process resources after their user goes away.

## Failure posture

No kasyncio path panics on user-controlled input: invalid input becomes
an error completion (the cookie is preserved), resource exhaustion becomes
`WouldBlock`/backoff, and teardown paths never block their callers.
