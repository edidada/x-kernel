---
id: CG-040-lock-stat-accounting
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: ai
    triggers:
      - '\b(?:LockClassStats|record_(?:[a-z]+_)?(?:acquisitions?|contentions?)|record_blocking)\b'
---

# CG-040-lock-stat-accounting: Record successful takes and actual lock contention

- **Rule**: Lock implementations MUST use
  `LockClassStats::record_acquisitions` and `record_contentions`, count one
  acquisition per successful take, and count one contention when a Mutex/RwLock
  waiter actually blocks or an SMP spin acquisition fails its first try,
  rather than counting every failed poll.
- **Scope**: Local acquisition-success and blocking/slow branches of instrumented
  lock implementations.
  No lint checks branch accounting. Lock-stat code compiles only with
  `KFEAT_LOCK_STAT=y`, which CI builds only in the AArch64 Dylint stage, so
  Clippy never lints it. AI judges each added record call, including the
  per-lock wrappers (`record_read_acquisitions`, `record_blocking` and
  similar), or `LockClassStats` use against the acquisition and contention
  branches.
  Not this rule: the Dylint lint `untracked_runtime_lock` ([CG-003](CG-003-runtime-lock-tracking.md)) owns runtime
  constructor tracking and [CG-001](CG-001-static-spinlock-registration.md)/
  the Dylint lint `unregistered_static_lock` ([CG-002](CG-002-static-mutex-registration.md)) own static registration.
  The `lock_stat_record_api_bypass` candidate in [pending.md](pending.md)
  checks field mutation outside record implementations, not branch accounting.
- **Why**: Incorrect acquisition or contention counts misrepresent lock
  behavior and undermine contention-based tuning.
- **How to check**:
  1. Trace each successful take and confirm exactly one acquisition record on that path.
  2. For Mutex/RwLock, record one contention when the waiter actually blocks,
     before block_on; for SMP spin acquisition, record once after the failed
     first try. Inspect local retries to exclude per-poll overcounting.
  3. Confirm updates use the record APIs. Consult
     [lock-stat](../ai/skills/performance-analysis/references/lock-stat.md)
     for the instrumentation contract.
- **Exceptions**: None for instrumented locks; an uncontended successful take records no contention.
- **Counter-example**:

```rust,ignore
while !try_take() {
    stats.record_contentions(1);
}
stats.record_acquisitions(1);
```

Each failed poll counts a new contention instead of recording the first failed try once.

- **Example**:

```rust,ignore
if !try_take() {
    stats.record_contentions(1);
    while !try_take() {
        core::hint::spin_loop();
    }
}
stats.record_acquisitions(1);
```
