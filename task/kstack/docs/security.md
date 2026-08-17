# kstack — Security

## Trust boundaries

Stack allocation sizes come from kernel-internal callers only (`ktask`); no
untrusted input reaches `alloc`/`free` directly.

## Invariants

- A `KernelStackMapping` is owned exclusively by its caller; `free` consumes
  it exactly once.
- The usable range handed to the task is fully mapped and writable; any guard
  region at the base is an unmapped VA hole provided by the backend.
- The per-CPU cache is only accessed on the owning CPU with preemption and
  IRQs disabled; cache slots never mix stack sizes.
- Backends are never invoked from inside the IRQ-off cache section. A
  task-context backend free may therefore block on a TLB shootdown; an
  IRQ-context free must be deferrable by the backend (vmalloc does this).

## Failure handling

- Registering a second backend panics (`init_once`) — a boot-order bug.
- `alloc` returns `KResult`; backends unwind partial mappings on failure.

