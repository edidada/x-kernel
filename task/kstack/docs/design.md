# kstack — Design

## Purpose

`kstack` owns the kernel-stack resource boundary shared by the task layer
(`ktask`, the consumer) and the address-space layer (`memspace`, the backend
provider). Keeping this boundary in a dedicated crate lets `ktask` allocate
and free kernel stacks without depending on `memspace`, while `memspace`
injects the actual allocator at boot — the dependency direction is:

```text
ktask -> kstack <- memspace
```

## Key types

- `KernelStackMapping`: an opaque handle to one stack allocation. It carries
  the virtual range, the guard-region size, and backend-owned page handles so
  `free` never has to walk the page table.
- `register_ops` / `alloc` / `free`: the dispatch layer. `memspace` registers
  one backend (heap-backed or vmalloc-backed with guard page) at boot.

## Per-CPU cache

Only stacks of `KERNEL_STACK_SIZE` are cached (two per CPU). They dominate the
allocation churn of user-task create/destroy; kernel threads with custom stack
sizes bypass the cache so a slot is never returned for the wrong size. Cached
stacks are scrubbed before reuse.

## Locking and context

The `NoPreemptIrqSave` guard covers **only** the per-CPU cache access, so the
cache is only touched by the current CPU and an IRQ handler can never re-enter
the same cache on the same CPU. Backend `alloc`/`free` run **outside** the
IRQ-off section, in the caller's own context.

This split is load-bearing: the vmalloc backend's free path may run a
synchronous TLB shootdown that spin-waits for remote CPUs to acknowledge an
IPI. Entering that wait with IRQs disabled means the local CPU cannot service
other CPUs' shootdown IPIs — two CPUs doing so concurrently deadlock
(see `docs/issues/2026-08-17-tlb-shootdown-deadlock.md`). IRQ-context frees
stay safe because the vmalloc backend detects exception context and defers
teardown to `system_wq` instead of shooting down in-place.
