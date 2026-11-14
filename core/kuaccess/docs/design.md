# kuaccess design

## Purpose

`kuaccess` is the kernel-side glue for accessing user memory:

- connect `osvm`'s generic virtual-memory access entry points to the current thread's address space;
- handle the trap path that lets the kernel resolve page faults while it is accessing a user address;
- provide a few hot user-string load helpers;
- provide user 32-bit atomic load and compare-exchange primitives for paths such as futex.

Callers include syscall implementations, the user-thread runtime, and other crates that use `osvm` pointer wrappers.

## Scope

The current scope is:

- `src/lib.rs`
- `ut-tests/copy_tests.rs` (user-copy regression tests)

## Architecture

```text
syscall / runtime
      |
      v
  kuaccess
   |     \
   |      \-- vm_load_string* -> osvm load helpers
   |      \-- atomic_load/cmpxchg_u32 -> architecture user atomics
   |
   \-- VirtMemIo(Vm) -> user_copy + current thread address space
```

## Calling constraints and execution context

- Must run in a context where a current task exists.
- `access_user_memory()` requires the current task to resolve to a thread.
- User-string loads access user addresses, so they may sleep and resolve page faults.
- Ordinary `osvm` copy entry points require a sleepable thread context and do not disable IRQs themselves. A page fault returns through the exception-table fixup, then the calling task fills the page back in. Filling a file page may wait for block I/O.
- The scoped raw-access trap adapter depends on the current thread's `accessing_user_memory` flag. Faultable atomic operations use task-context resolution instead of this window.

## Algorithms

### User memory access

1. The caller starts an access through an `osvm` pointer wrapper or `vm_load_string*()`.
2. `Vm` first checks that the user address is in range.
3. `copy_user_pages()` splits the copy on 4 KiB user-page boundaries and calls `user_copy` for each piece.
4. Ordinary copies do not open the in-trap fill window. On a fault, the exception-table fixup returns, and the calling task then calls the current process's `MmSpace::handle_page_fault()`. This avoids waiting for IRQ-driven disk completion while IRQs are masked in exception context.
5. One progress loop tries the current page fragment first. Success advances the offset and resets that page's fault count. Failure invokes the captured live MM's task-context resolver. On admission `Retry`, that resolver retains the current runtime, drops the MM mutex and waits for its resources. Each subsequent attempt performs fresh VMA lookup and permission checks. These waits do not spend the copy's materialization budget.
6. After `Resolved`, the copy may fault again so shared-file materialization and write permission can finish in stages. Completed/COW attempts that still do not enable a copy retain the per-page bound of 16. Explicit no-progress and permanent faults fail immediately. This protects against a backing provider repeatedly claiming progress without turning temporary lock contention into `NoAccess`. A successful return still means the entire output was initialized.

### User atomic access

1. Check 4-byte alignment and the user address range.
2. Both raw atomic primitives suppress an outer raw-access window under `IrqSave` and use the assembly exception-table fixup. The short guard ends before any waiting.
3. Faultable loads use the existing `copy_user_pages` progress loop: a failed atomic load enters the live MM task resolver, waits for admission with MM released, rechecks the VMA and repeats the load. Compare-exchange similarly resolves a failed nofault store in task context before retrying.
4. Success returns the observed value or `(exchanged, observed)`. Nofault variants immediately return `NoAccess` on a fault, including inside an outer access window. Fault-resolving variants return an error only when resolution fails or the existing copy progress bound is exhausted.

These primitives give futex and robust-list paths a TOCTOU-free read or update of a user futex word. A read-only load does not require a writable mapping and does not trigger COW.

`fault_in_user_writeable()` invokes the same task-context resolver without storing a user word. Admission waits release MM serialization, retain the captured active MM user and revalidate the current mapping afterwards. COW conflicts are returned to the caller's operation retry. OOM retains its distinct result; unmapped or denied addresses fail rather than being treated as progress. `FUTEX_WAKE_OP` uses this outside bucket locks before resolving keys again and retrying its nofault CAS.

### String loads

1. Read a byte vector or a NUL-terminated byte stream from the user address.
2. Validate UTF-8.
3. Failure returns `IllegalBytes`.

## Concurrency

- `Vm` does not use `IrqSave`. The slow fault path of an ordinary copy, like a disk read, requires a sleepable context. Nofault atomic access keeps its local `IrqSave`.
- There is no global shared state. Concurrency control belongs to thread state and the address-space lock.

## Design decisions

- User-string loads stay in `kuaccess` rather than a syscall crate because they are part of "how to access user memory safely", not of one syscall family.
- Only string-level helpers are kept. This crate does not grow into a general user-argument parsing layer.
- The trap handler's external ABI is still `bool`, but that bool is only the adapter result of architecture trap dispatch. MM semantics come from `PageFaultOutcome`, so `kuaccess` does not define its own fault classification.

## Regression tests

`ut-tests/copy_tests.rs` uses an on-demand mapping backend that requires `PreparedTaskWait`. It checks that ordinary-copy fault handling runs in the sleepable task context outside the exception, and covers cross-page reads, a first-touch write fault, read-only protection, and revoking a mapping. The staged write-fault test spans multiple pages and exceeds the per-page recovery budget, checking that the copy is complete after repeated faults on the same page. The no-progress tests cover a bounded exit when the backend keeps reporting success or a COW retry, and an immediate failure on an explicit `NoProgress`. The admission test also repeats a faultable atomic load through more than 16 provider waits, while the outer-window nofault test verifies that an atomic load does not enter the provider. The tests do not depend on real disk or IRQ timing.


### Locked copies and prefaulting

`Vm::write_mem_nofault` validates the range and splits architecture copies at
user-page boundaries. It accumulates completed fragments and stops on the first
short copy, so a bulk fixup cannot hide an earlier writable page's progress.
The returned prefix leaves the destination cursor on the page needing fault
preparation. It temporarily closes any outer user-fault access window and restores
that window after the copy.
It never invokes MM or waits for an inode lock. `Vm::fault_in_writeable` instead
checks VMA permissions and actual writable user PTEs under MM serialization, then
uses the existing task-context fault driver for missing permissions. The driver
waits for admission outside MM and retains its materialization progress bound.
Prefaulting never writes a probe value into the user's buffer and does not pin
the mapping. The caller must release source folio and backing-provider locks
before invoking it, then retry through the nofault copy operation.

### File-fault admission retry

The raw exception adapter reports admission `Retry` as unhandled so nofault
assembly reaches its fixup. The existing task-context resolver then waits with
MM released and rechecks the VMA. This also covers file invalidate contention;
an exception retry cannot itself make progress on a blocked truncate owner.

The raw adapter also uses try-MM admission. A lock holder may be awaiting this
CPU's TLB acknowledgement, so an IRQ-masked exception must reach the nofault
fixup instead of sleeping on that MM. The active MM capability survives fixup.
