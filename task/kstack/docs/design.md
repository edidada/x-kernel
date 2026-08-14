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

`alloc`/`free` run with preemption and IRQs disabled
(`NoPreemptIrqSave`) so the per-CPU cache is only touched by the current CPU
and an IRQ handler can never re-enter the same cache on the same CPU.
Backend implementations (see `memspace`) must respect the calling context:
stack free can happen at task-destruction time or from an IRQ handler, so
backends must not rely on interruptible context.
