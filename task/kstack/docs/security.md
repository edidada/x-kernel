# kstack — Security

## Trust boundaries

Stack allocation sizes come from kernel-internal callers only (`ktask`); no
untrusted input reaches `alloc`/`free` directly.

## Invariants

- A `KernelStackMapping` is owned exclusively by its caller; `free` consumes
  it exactly once.
- The usable range handed to the task is fully mapped and writable; any guard
  region at the base is an unmapped VA hole provided by the backend.
- The per-CPU cache is only accessed on the owning CPU with preemption
  disabled; cache slots never mix stack sizes.

## Failure handling

- Registering a second backend panics (`init_once`) — a boot-order bug.
- `alloc` returns `KResult`; backends unwind partial mappings on failure.

